use native;

cpp! {{
    #include <sailfishapp.h>

    struct SailfishApplicationHolder : QmlApplicationHolder {
        SailfishApplicationHolder(int &argc, char **argv)
            : QmlApplicationHolder(argc, argv),
              app(SailfishApp::application(argc, argv)),
              view(SailfishApp::createView()) {
                self->view->setSource(SailfishApp::pathTo("qml/harbour-whisperfish.qml"));

                QObject::connect(
                    view->engine(),
                    &QQmlEngine::quit,
                    view.get(),
                    &QWindow::close,
                    Qt::QueuedConnection
                );
            }
    };
}}

cpp_class! (
    pub unsafe struct SailfishApp as "SailfishApplicationHolder"
);

impl QmlAppTrait for SailfishApp {}

impl SailfishApp {
    pub fn install_default_translator(&mut self) -> Result<(), anyhow::Error> {
        let result = unsafe {
            cpp!([self as "QmlApplicationHolder*"] -> u32 as "int" {
                const QString transDir = SailfishApp::pathTo(QStringLiteral("translations")).toLocalFile();
                const QString appName = qApp->applicationName();
                QTranslator translator(qApp);
                int result = 0;
                if (!translator.load(QLocale(), appName, "-", transDir)) {
                    qWarning() << "Failed to load translator for" << QLocale::system().uiLanguages()
                               << "Searched" << transDir << "for" << appName;
                    result = 1;
                    if(!translator.load(appName, transDir)) {
                        qWarning() << "Could not load default translator either!";
                        result = 2;
                    }
                }
                self->app->installTranslator(&translator);
                return result;
            })
        };
        match result {
            0 => Ok(()),
            1 => {
                log::info!("Default translator loaded.");
                Ok(())
            }
            2 => anyhow::bail!("No translators found"),
            _ => unreachable!("Impossible return code from C++"),
        }
    }
}
