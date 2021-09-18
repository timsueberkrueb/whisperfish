#[cfg(feature = "sailfish")]
mod native;

#[cfg(not(feature = "sailfish"))]
mod universal;

#[cfg(feature = "sailfish")]
pub use native::*;

// #[cfg(not(feature = "sailfish"))]
// pub type QmlApp = qmetaobject::QmlEngine;

#[cfg(not(feature = "sailfish"))]
pub use universal::*;

pub mod qrc;
