//! Run [NSView new] on a separate thread, which should get caught by the
//! main thread checker.
use objc2::rc::Id;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send_id};

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

fn main() {
    std::thread::scope(|s| {
        s.spawn(|| {
            // Note: Usually you'd use `icrate::NSView::new`, this is to
            // avoid the heavy dependency.
            let _: Id<AnyObject> = unsafe { msg_send_id![class!(NSView), new] };
        });
    });
}
