package main

import (
	"database/sql"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	log "github.com/Sirupsen/logrus"
	"github.com/godbus/dbus"
	"github.com/janimo/textsecure"
	"github.com/janimo/textsecure/3rd_party/magic"
	"github.com/jmoiron/sqlx"
	_ "github.com/mattn/go-sqlite3"
	"github.com/ttacon/libphonenumber"
	"gopkg.in/qml.v1"
)

const (
	Version                = "0.1.1"
	Appname                = "harbour-whisperfish"
	PageStatusInactive     = 0
	PageStatusActivating   = 1
	PageStatusActive       = 2
	PageStatusDeactivating = 3
)

type Whisperfish struct {
	window          *qml.Window
	engine          *qml.Engine
	contactsModel   Contacts
	sessionModel    SessionModel
	messageModel    MessageModel
	configDir       string
	configFile      string
	dataDir         string
	storageDir      string
	attachDir       string
	settings        *Settings
	config          *textsecure.Config
	db              *sqlx.DB
	activeSessionID int64
}

func main() {
	if err := qml.SailfishRun(Appname, "", Version, runGui); err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Fatal("Sailfish application failed")
	}
}

func NewDb(path string) (*sqlx.DB, error) {
	db, err := sqlx.Open("sqlite3", path)
	if err != nil {
		return nil, err
	}

	err = db.Ping()
	if err != nil {
		return nil, err
	}

	_, err = db.Exec(SessionSchema)
	if err != nil {
		return nil, err
	}

	_, err = db.Exec(MessageSchema)
	if err != nil {
		return nil, err
	}

	return db, nil
}

func runGui() error {
	whisperfish := Whisperfish{}
	whisperfish.Init(qml.SailfishNewEngine())

	controls, err := whisperfish.engine.SailfishSetSource("qml/harbour-whisperfish.qml")
	if err != nil {
		return err
	}

	window := controls.SailfishCreateWindow()
	whisperfish.window = window

	window.SailfishShow()

	go whisperfish.runBackend()

	window.Wait()

	return nil
}

// Runs backend
func (w *Whisperfish) runBackend() {
	client := &textsecure.Client{
		GetConfig:           func() (*textsecure.Config, error) { return w.getConfig() },
		GetPhoneNumber:      func() string { return w.getPhoneNumber() },
		GetVerificationCode: func() string { return w.getVerificationCode() },
		GetStoragePassword:  func() string { return w.getStoragePassword() },
		MessageHandler:      func(msg *textsecure.Message) { w.messageHandler(msg) },
		ReceiptHandler:      func(source string, devID uint32, timestamp uint64) { w.receiptHandler(source, devID, timestamp) },
		RegistrationDone:    func() { w.registrationDone() },
		GetLocalContacts:    getSailfishContacts,
	}

	err := textsecure.Setup(client)
	if _, ok := err.(*strconv.NumError); ok {
		os.RemoveAll(w.storageDir)
		log.Fatal("Switching to unencrypted session store, removing %s\nThis will reset your sessions and reregister your phone.", w.storageDir)
	}
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Fatal("Failed to setup textsecure client")
		return
	}

	w.RefreshContacts()
	w.RefreshSessions()

	for {
		if err := textsecure.StartListening(); err != nil {
			log.WithFields(log.Fields{
				"error": err,
			}).Error("Error processing Websocket event from Signal")
			time.Sleep(3 * time.Second)
		}
	}
}

// Refresh contacts
func (w *Whisperfish) RefreshContacts() {
	w.contactsModel.Refresh()
}

// Refresh session model
func (w *Whisperfish) RefreshSessions() {
	w.sessionModel.Length = 0
	qml.Changed(&w.sessionModel, &w.sessionModel.Length)

	err := w.sessionModel.Refresh(w.db, &w.contactsModel)
	if err != nil && err != sql.ErrNoRows {
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to fetch sessions from database")
	}

	qml.Changed(&w.sessionModel, &w.sessionModel.Length)
}

// Set active session
func (w *Whisperfish) SetSession(sessionID int64) {
	session, err := FetchSession(w.db, sessionID)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
			"id":    sessionID,
		}).Error("Failed to fetch session")
	}

	w.activeSessionID = sessionID
	if session.IsGroup {
		w.messageModel.Name = session.GroupName
	} else {
		w.messageModel.Name = w.contactsModel.Name(session.Source)
	}
	w.messageModel.Tel = session.Source
	qml.Changed(&w.messageModel, &w.messageModel.Name)
	qml.Changed(&w.messageModel, &w.messageModel.Tel)

	err = MarkSessionRead(w.db, sessionID)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
			"sid":   w.activeSessionID,
		}).Error("Failed to mark session read")
	}
}

// Refresh conversation model
func (w *Whisperfish) RefreshConversation() {
	w.messageModel.Length = 0
	qml.Changed(&w.messageModel, &w.messageModel.Length)

	err := w.messageModel.RefreshConversation(w.db, w.activeSessionID)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to fetch messages from database")
	}

	qml.Changed(&w.messageModel, &w.messageModel.Length)
}

// Initializes Whisperfish application and qml context
func (w *Whisperfish) Init(engine *qml.Engine) {
	w.engine = engine
	w.engine.Translator(fmt.Sprintf("/usr/share/%s/qml/i18n", Appname))

	w.configDir = filepath.Join(w.engine.SailfishGetConfigLocation(), Appname)
	w.dataDir = w.engine.SailfishGetDataLocation()
	w.storageDir = filepath.Join(w.dataDir, "storage")
	w.attachDir = filepath.Join(w.storageDir, "attachments")
	dbDir := filepath.Join(w.dataDir, "db")
	dbFile := filepath.Join(dbDir, fmt.Sprintf("%s.db", Appname))

	os.MkdirAll(w.configDir, 0700)
	os.MkdirAll(w.dataDir, 0700)
	os.MkdirAll(w.attachDir, 0700)
	os.MkdirAll(dbDir, 0700)

	settingsFile := filepath.Join(w.configDir, "settings.yml")
	w.settings = &Settings{}

	if err := w.settings.Load(settingsFile); err != nil {
		w.settings.SetDefault()
		// write out default settings file
		if err = w.settings.Save(settingsFile); err != nil {
			log.WithFields(log.Fields{
				"error": err,
			}).Error("Failed to write out default settings file")
		}
	}

	var err error
	w.db, err = NewDb(dbFile)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to open database")
	}

	// initialize model delegates
	w.engine.Context().SetVar("whisperfish", w)
	w.engine.Context().SetVar("contactsModel", &w.contactsModel)
	w.engine.Context().SetVar("sessionModel", &w.sessionModel)
	w.engine.Context().SetVar("messageModel", &w.messageModel)
}

// Returns the GO runtime version used for building the application
func (w *Whisperfish) RuntimeVersion() string {
	return runtime.Version()
}

// Returns the Whisperfish application version
func (w *Whisperfish) Version() string {
	return Version
}

// Get the config file for Signal
func (w *Whisperfish) getConfig() (*textsecure.Config, error) {
	w.configFile = filepath.Join(w.configDir, "config.yml")
	var errConfig error
	if _, err := os.Stat(w.configFile); err == nil {
		w.config, errConfig = textsecure.ReadConfig(w.configFile)
	} else {
		w.config = &textsecure.Config{}
	}

	w.config.StorageDir = w.storageDir
	w.config.UserAgent = fmt.Sprintf("Whisperfish v%s", Version)
	w.config.UnencryptedStorage = true
	w.config.LogLevel = "debug"
	w.config.AlwaysTrustPeerID = true
	rootCA := filepath.Join(w.configDir, "rootCA.crt")
	if _, err := os.Stat(rootCA); err == nil {
		w.config.RootCA = rootCA
	}
	return w.config, errConfig
}

// Prompt the user for storage password
func (w *Whisperfish) getStoragePassword() string {
	pass := w.getTextFromDialog("getStoragePassword", "passwordDialog", "passwordEntered")
	log.Printf("Password: %s", pass)

	return pass
}

// Prompt the user to enter the verification code
func (w *Whisperfish) getVerificationCode() string {
	code := w.getTextFromDialog("getVerificationCode", "verifyDialog", "codeEntered")
	log.Printf("Code: %s", code)

	return code
}

// Prompt the user to enter telephone number for Registration
func (w *Whisperfish) getPhoneNumber() string {
	n := w.getTextFromDialog("getPhoneNumber", "registerDialog", "numberEntered")
	num, err := libphonenumber.Parse(fmt.Sprintf("+%s", n), "")
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to parse phone number")
	}

	tel := libphonenumber.Format(num, libphonenumber.E164)
	log.Printf("Using phone number: %s", tel)
	return tel
}

// Registration done
func (w *Whisperfish) registrationDone() {
	textsecure.WriteConfig(w.configFile, w.config)

	log.Println("Registered")
	status := w.getCurrentPageStatus()
	for status == PageStatusActivating || status == PageStatusDeactivating {
		// If current page is in transition need to wait before pushing dialog on stack
		time.Sleep(100 * time.Millisecond)
		status = w.getCurrentPageStatus()
	}
	w.window.Root().ObjectByName("main").Call("registered")
}

// Get the current page status
func (w *Whisperfish) getCurrentPageStatus() int {
	return w.window.Root().ObjectByName("main").Object("currentPage").Int("status")
}

// Get the current page id
func (w *Whisperfish) getCurrentPageID() string {
	return w.window.Root().ObjectByName("main").Object("currentPage").String("objectName")
}

// Get text from dialog window
func (w *Whisperfish) getTextFromDialog(fun, obj, signal string) string {
	status := w.getCurrentPageStatus()
	for status == PageStatusActivating || status == PageStatusDeactivating {
		// If current page is in transition need to wait before pushing dialog on stack
		time.Sleep(100 * time.Millisecond)
		status = w.getCurrentPageStatus()
	}

	w.window.Root().ObjectByName("main").Call(fun)
	p := w.window.Root().ObjectByName(obj)
	ch := make(chan string)
	p.On(signal, func(text string) {
		ch <- text
	})
	text := <-ch
	return text
}

// Message handler
func (w *Whisperfish) messageHandler(msg *textsecure.Message) {
	log.Printf("Received message from: %s", msg.Source())

	message := &Message{
		Source:    msg.Source(),
		Message:   msg.Message(),
		Timestamp: time.Now(),
		Flags:     msg.Flags(),
	}

	if len(msg.Attachments()) > 0 {
		if w.settings.SaveAttachments {
			err := message.SaveAttachment(w.attachDir, msg.Attachments()[0])
			if err != nil {
				log.WithFields(log.Fields{
					"error": err,
				}).Error("Failed to save attachment")
			}
		} else {
			message.HasAttachment = true
			message.MimeType = msg.Attachments()[0].MimeType
		}
	}

	session, err := w.sessionModel.Add(w.db, message, msg.Group(), true)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to add message to database")
		return
	}

	if w.activeSessionID == session.ID {
		w.RefreshConversation()
		if w.getCurrentPageID() == "conversation" {
			err := MarkSessionRead(w.db, session.ID)
			if err != nil {
				log.WithFields(log.Fields{
					"error": err,
					"sid":   w.activeSessionID,
				}).Error("Failed to mark session read")
			}
		}
	}

	w.RefreshSessions()

	if w.settings.EnableNotify {
		w.notify(msg)
	}
}

// Send new message notification
// From https://lists.sailfishos.org/pipermail/devel/2016-April/007036.html
func (w *Whisperfish) notify(msg *textsecure.Message) error {
	conn, err := dbus.SessionBus()
	if err != nil {
		return err
	}

	title := w.contactsModel.Name(msg.Source())
	body := "New message"

	var m map[string]dbus.Variant
	m = make(map[string]dbus.Variant)
	m["category"] = dbus.MakeVariant("x-nemo.messaging.im")

	obj := conn.Object("org.freedesktop.Notifications", "/org/freedesktop/Notifications")
	call := obj.Call("org.freedesktop.Notifications.Notify", 0, "",
		uint32(0),
		"", title, body, []string{},
		m,
		int32(0))
	if call.Err != nil {
		return err
	}
	return nil
}

// Send message
func (w *Whisperfish) SendMessage(source, message, groupName, attachment string) {
	var err error

	m := strings.Split(source, ",")
	if len(m) > 1 {
		group, err := textsecure.NewGroup(groupName, m)
		if err != nil {
			log.WithFields(log.Fields{
				"error":      err,
				"group_name": groupName,
			}).Error("Failed to create new group")
			return
		}

		err = w.sendMessageHelper(group.Hexid, message, attachment, group)
	} else {
		err = w.sendMessageHelper(source, message, attachment, nil)
	}

	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
			"sid":   w.activeSessionID,
		}).Error("Failed to send message")
	}
}

func (w *Whisperfish) sendMessageHelper(to, msg, attachment string, group *textsecure.Group) error {
	message := &Message{
		Source:    to,
		Message:   msg,
		Timestamp: time.Now(),
		Sent:      true,
	}

	if len(attachment) > 0 {
		att, err := os.Open(attachment)
		if err != nil {
			return err
		}
		defer att.Close()
		//XXX Sucks we have to do this twice
		message.MimeType, _ = magic.MIMETypeFromReader(att)
		message.Attachment = attachment
		message.HasAttachment = true
	}

	session, err := w.sessionModel.Add(w.db, message, group, false)
	if err != nil {
		return err
	}

	w.activeSessionID = session.ID
	if session.IsGroup {
		w.messageModel.Name = session.GroupName
	} else {
		w.messageModel.Name = w.contactsModel.Name(session.Source)
	}
	w.messageModel.Tel = session.Source
	qml.Changed(&w.messageModel, &w.messageModel.Name)
	qml.Changed(&w.messageModel, &w.messageModel.Tel)
	w.RefreshConversation()
	w.RefreshSessions()

	go w.sendMessage(session, message)
	return nil
}

func (w *Whisperfish) sendMessage(s *Session, m *Message) {
	var att io.Reader
	var err error

	if m.Attachment != "" {
		att, err = os.Open(m.Attachment)
		if err != nil {
			return
		}
	}

	ts := w.sendMessageLoop(s.Source, m.Message, s.IsGroup, att, m.Flags)

	timestamp := time.Unix(0, int64(1000000*ts)).Local()

	err = MarkSessionSent(w.db, s.ID, m.Message, timestamp)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
			"id":    s.ID,
		}).Error("Failed to mark session sent")
	}

	err = MarkMessageSent(w.db, m.ID, timestamp)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
			"id":    m.ID,
		}).Error("Failed to mark message sent")
	}

	w.RefreshConversation()
	w.RefreshSessions()
}

func (w *Whisperfish) sendMessageLoop(to, message string, group bool, att io.Reader, flags uint32) uint64 {
	var err error
	var ts uint64
	for {
		err = nil
		if flags == textsecure.EndSessionFlag {
			ts, err = textsecure.EndSession(to, "TERMINATE")
		} else if flags == textsecure.GroupLeaveFlag {
			err = textsecure.LeaveGroup(to)
		} else if flags == textsecure.GroupUpdateFlag {
			// TODO: implement me
			//_, err = textsecure.UpdateGroup(to, groups[to].Name, strings.Split(groups[to].Members, ","))
		} else if att == nil {
			if group {
				ts, err = textsecure.SendGroupMessage(to, message)
			} else {
				ts, err = textsecure.SendMessage(to, message)
			}
		} else {
			if group {
				ts, err = textsecure.SendGroupAttachment(to, message, att)
			} else {
				ts, err = textsecure.SendAttachment(to, message, att)
			}
		}
		if err == nil {
			break
		}
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to send message")
		//If sending failed, try again after a while
		time.Sleep(3 * time.Second)
	}
	return ts
}

// Receipt handler
func (w *Whisperfish) receiptHandler(source string, devID uint32, ts uint64) {
	log.Printf("Receipt handler source %s timestamp %d", source, ts)

	timestamp := time.Unix(0, int64(1000000*ts)).Local()

	sessionID, err := MarkMessageReceived(w.db, source, timestamp)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
		}).Error("Failed to mark message received")
	}

	err = MarkSessionReceived(w.db, sessionID, timestamp)
	if err != nil {
		log.WithFields(log.Fields{
			"error": err,
			"id":    sessionID,
		}).Error("Failed to mark session received")
	}

	if w.activeSessionID == sessionID {
		w.RefreshConversation()
	}

	w.RefreshSessions()
}
