#pragma once

#include <QtCore>

class Settings : public QObject {
    Q_OBJECT

public:
    Settings(QObject *parent = nullptr): QObject(parent) {
    }

    void setup();
    void setDefaults();

public slots:
    bool boolValue(const QString key) const;
    QString stringValue(const QString key) const;

    void boolSet(const QString key, bool val);
    void stringSet(const QString key, const QString val);
};
