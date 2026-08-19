use std::env;
use std::path::PathBuf;

#[path = "../../build_support/libclang.rs"]
mod libclang;

fn main() {
    libclang::ensure();
    let bindings = bindgen::Builder::default()
        .header("libkrun_input.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("input_header.rs"))
        .expect("Couldn't write bindings!");
}
