use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use regex::Regex;

use ts_rs::TS;

#[path = "src/lib/stream/webrtc/signalling_protocol.rs"]
mod signalling_protocol;

use crate::signalling_protocol::*;

fn file_download(url: &str, output: &str) {
    let mut resp =
        reqwest::blocking::get(url).unwrap_or_else(|_| panic!("Failed to download file: {url}"));
    let file_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(output);
    let mut output_file = std::fs::File::create(&file_path)
        .unwrap_or_else(|_| panic!("Failed to create file: {file_path:?}"));
    std::io::copy(&mut resp, &mut output_file).expect("Failed to copy content.");
}

#[cfg(windows)]
fn print_link_search_path() {
    use std::env;

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    if cfg!(target_arch = "x86_64") {
        println!("cargo:rustc-link-search=native={}/lib/x64", manifest_dir);
    } else {
        println!("cargo:rustc-link-search=native={}/lib", manifest_dir);
    }
}

#[cfg(not(windows))]
fn print_link_search_path() {}

fn main() {
    print_link_search_path();

    // Configure vergen
    let mut config = vergen::Config::default();
    *config.build_mut().semver_mut() = true;
    *config.build_mut().timestamp_mut() = true;
    *config.build_mut().kind_mut() = vergen::TimestampKind::DateOnly;
    *config.git_mut().sha_kind_mut() = vergen::ShaKind::Short;

    vergen::vergen(config).expect("Unable to generate the cargo keys!");

    file_download(
        "https://unpkg.com/vue@3.0.5/dist/vue.global.js",
        "src/html/vue.js",
    );

    // set SKIP_BINDINGS=1 to skip typescript bindings generation
    if std::env::var("SKIP_BINDINGS").is_err() {
        generate_typescript_bindings();
    }

    // set SKIP_BUN=1 to skip bun build
    if std::env::var("SKIP_WEB").is_err() {
        build_web();
    }
}

fn generate_typescript_bindings() {
    println!("cargo:rerun-if-changed=src/lib/stream/webrtc/signalling_protocol.rs");
    // Generate all typescript bindings and join them into a single String
    let bindings = [
        Message::export_to_string().unwrap(),
        Answer::export_to_string().unwrap(),
        Question::export_to_string().unwrap(),
        Negotiation::export_to_string().unwrap(),
        BindOffer::export_to_string().unwrap(),
        BindAnswer::export_to_string().unwrap(),
        PeerIdAnswer::export_to_string().unwrap(),
        Stream::export_to_string().unwrap(),
        IceNegotiation::export_to_string().unwrap(),
        MediaNegotiation::export_to_string().unwrap(),
        EndSessionQuestion::export_to_string().unwrap(),
    ]
    .join("\n\n");
    // Remove all typescript "import type" because all types are going to live in the same typescritp file
    let re = Regex::new(r"(?m)^import type .*\n").unwrap();
    let bindings = re.replace_all(bindings.as_str(), "").to_string();
    // Replace all notices by a custom one
    let re = Regex::new(r"(?m)^// This file was generated by .*\n\n").unwrap();
    let mut bindings = re.replace_all(bindings.as_str(), "").to_string();
    let custom_notice_str = "// This file was generated by [ts-rs](https://github.com/Aleph-Alpha/ts-rs) during `cargo build` step. Do not edit this file manually.\n\n";
    bindings.insert_str(0, custom_notice_str);
    // Export to file
    let output_dir = Path::new("./src/lib/stream/webrtc/frontend/bindings/");
    if !output_dir.exists() {
        std::fs::create_dir_all(output_dir).unwrap();
    }
    let bindings_file_path = output_dir.join(Path::new("signalling_protocol.d.ts"));
    let mut bindings_file = fs::File::create(bindings_file_path).unwrap();
    bindings_file.write_all(bindings.as_bytes()).unwrap();
}

fn build_web() {
    // Note that as we are not waching all files, sometimes we'd need to force this build
    println!("cargo:rerun-if-changed=./src/lib/stream/webrtc/frontend/index.html");
    println!("cargo:rerun-if-changed=./src/lib/stream/webrtc/frontend/package.json");
    println!("cargo:rerun-if-changed=./src/lib/stream/webrtc/frontend/src");

    let frontend_dir = Path::new("./src/lib/stream/webrtc/frontend");
    frontend_dir.try_exists().unwrap();

    let program = if Command::new("bun")
        .args(["--version"])
        .status()
        .ok()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        "bun"
    } else {
        "yarn"
    };

    let version = Command::new(program)
        .args(["--version"])
        .status()
        .unwrap_or_else(|_| {
            panic!("Failed to build frontend, `{program}` appears to be not installed.",)
        });

    if !version.success() {
        panic!("{program} version failed!");
    }

    let install = Command::new(program)
        .args(["install", "--frozen-lockfile"])
        .current_dir(frontend_dir)
        .status()
        .unwrap();

    if !install.success() {
        panic!("{program} install failed!");
    }

    let args = if program == "bun" {
        vec!["run", "build"]
    } else {
        vec!["build"]
    };
    let build = Command::new(program)
        .args(&args)
        .current_dir(frontend_dir)
        .status()
        .unwrap();

    if !build.success() {
        panic!("{program} build failed!");
    }
}
