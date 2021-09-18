import QtQuick 2.0
import QtQuick.Controls 2.0

ApplicationWindow {
    id: app

    title: "Whisperfish"
    visible: true

    Label {
        anchors.centerIn: parent
        text: "Hello World!"
    }

    Component.onCompleted: app.show()
}
