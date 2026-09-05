#[link(wasm_import_module = "app_capabilities")]
unsafe extern "C" {
    fn storage_write(path_ptr: *const u8, path_len: usize, value_ptr: *const u8, value_len: usize);
}

#[unsafe(no_mangle)]
pub extern "C" fn handle_request() {
    const PATH: &[u8] = b"/data/counter";
    const VALUE: &[u8] = b"1";
    unsafe {
        storage_write(PATH.as_ptr(), PATH.len(), VALUE.as_ptr(), VALUE.len());
    }
}
