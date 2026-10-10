use std::env;
use std::fs;
use std::path::PathBuf;

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn main() {
    println!("cargo:rerun-if-env-changed=BTC_SANDBOX_HELPER");
    println!("cargo:rerun-if-env-changed=BTC_REQUIRE_BUNDLED_HELPER");
    let release = env::var("PROFILE").is_ok_and(|profile| profile == "release")
        || env::var_os("BTC_REQUIRE_BUNDLED_HELPER").is_some();
    let destination = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("sandbox-helper");
    match env::var_os("BTC_SANDBOX_HELPER") {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            let bytes = fs::read(&path).expect("Read BTC_SANDBOX_HELPER");
            assert!(
                bytes.len() >= 64 && &bytes[..4] == b"\x7fELF" && bytes[4] == 2 && bytes[5] == 1,
                "Sandbox helper must be a little-endian 64-bit ELF"
            );
            let expected = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
                "x86_64" => 62,
                "aarch64" => 183,
                architecture => panic!("Unsupported Linux release architecture {architecture}"),
            };
            assert_eq!(
                u16_at(&bytes, 18),
                expected,
                "Sandbox helper architecture differs from executable target"
            );
            let start = usize::try_from(u64_at(&bytes, 32)).expect("ELF program table offset");
            let stride = usize::from(u16_at(&bytes, 54));
            let count = usize::from(u16_at(&bytes, 56));
            assert!(
                stride >= 4
                    && start
                        .checked_add(stride.checked_mul(count).expect("ELF table size"))
                        .is_some_and(|end| end <= bytes.len()),
                "Invalid helper ELF program headers"
            );
            if release {
                assert!(
                    (0..count).all(|i| u32::from_le_bytes(
                        bytes[start + i * stride..start + i * stride + 4]
                            .try_into()
                            .unwrap()
                    ) != 3),
                    "Release sandbox helper must be static (no ELF interpreter)"
                );
            }
            fs::write(&destination, bytes).expect("Embed sandbox helper");
        }
        None => {
            assert!(
                !release,
                "Release builds require BTC_SANDBOX_HELPER pointing to the target's static bubblewrap 0.13.0 binary. Use tools/build-release.sh."
            );
            fs::write(&destination, []).expect("Write development helper marker");
            println!(
                "cargo:warning=Development build has no embedded sandbox helper; supply BTC_SANDBOX_HELPER at build time or an explicit helper override at runtime. Production decisions fail closed without isolation."
            );
        }
    }
    println!(
        "cargo:rustc-env=BTC_SANDBOX_HELPER={}",
        destination.display()
    );
    println!("cargo:rustc-env=BTC_BUBBLEWRAP_VERSION=0.13.0");
}
