#![no_std]
use core::ffi::c_void;

type TryCatchClosure = extern "C-unwind" fn(*mut c_void);

extern "C-unwind" {
    #[link_name = "objc2_exception_helper_0_1_try_catch"]
    pub fn try_catch(f: TryCatchClosure, context: *mut c_void, error: *mut *mut c_void) -> u8;
}
