extern "C" {
    fn probe_cxx_len(text: *const std::ffi::c_char) -> i32;
}

fn main() {
    let length = unsafe { probe_cxx_len(c"probe".as_ptr()) };
    println!("check-shell probe: rust ok ({length})");
}
