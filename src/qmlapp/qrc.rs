use qmetaobject::*;

qrc!(qml_resources,
    "/" {
        "qml/UTTest.qml"
    }
);

pub fn load() {
    qml_resources();
}
