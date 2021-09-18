use qmetaobject::*;

pub struct QmlApp(QmlEngine);

cpp! {{
    #include <QtQuick/QtQuick>
}}

impl QmlApp {
    pub fn application(_name: String) -> Self {
        // FIXME: Check if we need to handle _name/customize something here
        Self(QmlEngine::new())
    }

    pub fn set_title(&mut self, title: QString) {
        // FIXME: Implement properly
        println!("TODO: implement set_title {}", title);
    }

    pub fn set_application_version(&mut self, version: QString) {
        // FIXME: Implement properly
        println!("TODO: implement {}", version);
    }

    pub fn install_default_translator(&mut self) -> Result<(), anyhow::Error> {
        // FIXME: Implement properly
        println!("TODO: implement install_default_translator");
        Ok(())
    }

    pub fn set_source(&mut self, src: QUrl) {
        self.0.load_file(QString::from(src))
    }

    #[allow(dead_code)]
    pub fn exec(&self) {
        self.0.exec()
    }

    pub fn show(&self) {
        // FIXME: Implement properly
        self.0.exec()
    }

    pub fn show_full_screen(&self) {
        // FIXME: Implement properly
        self.0.exec()
    }

    pub fn path_to(path: QString) -> QUrl {
        // FIXME: Check where this is used: app dir only or also data dirs?
        let app_dir = std::env::var("APP_DIR").expect("Failed to read the APP_DIR environment variable");
        let app_dir_path = std::path::PathBuf::from(app_dir);
        let res_path = app_dir_path.join(path.to_string());

        QUrl::from(QString::from(res_path.to_str().unwrap()))
    }

    /// Sets a property for this QML context (calls QQmlEngine::rootContext()->setContextProperty)
    ///
    // (TODO: consider making the lifetime the one of the engine, instead of static)
    pub fn set_object_property<T: QObject + Sized>(
        &mut self,
        name: QString,
        obj: QObjectPinned<'_, T>,
    ) {
        self.0.set_object_property(name, obj)
    }

    // TODO: these methods come directly from `qmetaobject::QmlEngine`.  Some form of attribution
    // is necessary, and some form casting into QmlEngine.  impl Deref<Target=QmlEngine> would be
    // ideal.
    /// Sets a property for this QML context (calls QQmlEngine::rootContext()->setContextProperty)
    pub fn set_property(&mut self, name: QString, value: QVariant) {
        self.0.set_property(name, value)
    }
}
