//! Run [NSView new] on a separate thread, which should get caught by the
//! main thread checker.
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

fn main() {
    std::thread::scope(|s| {
        s.spawn(|| {
            // Note: Usually you'd use `icrate::NSView::new`, this is to
            // avoid the heavy dependency.
            let _: Retained<AnyObject> = unsafe { msg_send![class!(NSView), new] };
        });
    });
}
