//! Spider Charts engine — universe, data, patterns and storage.
//!
//! Everything except the user interface lives here, so a front end is a thin
//! shell over it rather than the place the logic accumulates. The egui desktop
//! app and the Tauri/React app are both consumers of exactly this API.

pub mod bhavcopy;
pub mod config;
pub mod model;
pub mod patterns;
pub mod store;
pub mod sync;
pub mod ta;
pub mod universe;
pub mod upstox;
pub mod writelock;
