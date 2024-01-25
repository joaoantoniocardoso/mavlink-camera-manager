use gst::glib;
use gstrswebrtc::signaller::Signallable;

mod imp {
    use super::*;

    use gst::subclass::prelude::*;

    use gstrswebrtc::signaller::{Signallable, SignallableImpl};

    #[derive(Default)]
    pub struct Signaller {}

    #[glib::object_subclass]
    impl ObjectSubclass for Signaller {
        const NAME: &'static str = "MavlinkCameraManagerWebRTCSinkSignaller";
        type Type = super::Signaller;
        type ParentType = glib::Object;
        type Interfaces = (Signallable,);
    }

    impl ObjectImpl for Signaller {}

    impl SignallableImpl for Signaller {}

    impl GstObjectImpl for Signaller {}
}

glib::wrapper! {
    pub struct Signaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

unsafe impl Send for Signaller {}
unsafe impl Sync for Signaller {}
