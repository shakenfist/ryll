// The egui call sites for the app's panels, windows and overlays.
//
// eframe 0.34 replaced the old `update()` trait method with `ui()`,
// which hands us a root `&mut Ui` that fills the viewport. egui 0.35
// then unified `TopBottomPanel`/`SidePanel` into a single `Panel`
// type and made every panel's `show` take that `&mut Ui` instead of
// a `&Context` (`Window`s still take `&Context`). The panel/menu/
// frame call sites in this directory carry per-change notes; this
// completes the migration formerly tracked in
// `docs/plans/PLAN-egui-0.34-followups.md`.

mod cursor;
mod dialogs;
mod keys;
mod notifications_panel;
mod region_select;
mod stats_bar;
mod surface;
mod traffic_panel;
mod usb_panel;
mod webdav_panel;

pub(in crate::app) use traffic_panel::TrafficViewer;
pub(in crate::app) use usb_panel::UsbPanel;
pub(in crate::app) use webdav_panel::WebdavPanel;
