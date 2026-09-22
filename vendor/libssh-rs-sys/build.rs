use std::path::PathBuf;

fn main() {
    if std::env::var_os("CARGO_FEATURE_VENDORED").is_none()
        && pkg_config::Config::new()
            // ssh_userauth_publickey_auto_get_current_identity
            // is not yet in a released version of libssh
            .atleast_version("0.9.7")
            .probe("libssh")
            .is_ok()
    {
        return;
    }

    let mut cfg = cc::Build::new();
    cfg.define("LIBSSH_STATIC", None);
    cfg.include("vendored/include");
    cfg.flag_if_supported("-Wno-deprecated-declarations");

    let dst = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let include = dst.join("include");
    std::fs::create_dir_all(&include).unwrap();
    cfg.include(&include);
    println!("cargo:include={}", include.display());
    println!("cargo:root={}", dst.display());

    let openssl_version = std::env::var("DEP_OPENSSL_VERSION_NUMBER").unwrap();
    let openssl_version = u64::from_str_radix(&openssl_version, 16).unwrap();

    let target = std::env::var("TARGET").unwrap();

    let target_family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap();
    if target.contains("windows") {
        cfg.define("GLOBAL_CONF_DIR", Some("\"C:/ProgramData/ssh\""));
    } else {
        cfg.define("GLOBAL_CONF_DIR", Some("\"/etc/ssh\""));
    }
    cfg.define("GLOBAL_CLIENT_CONFIG", Some("\"/etc/ssh/ssh_config\""));
    cfg.define(
        "GLOBAL_BIND_CONFIG",
        Some("\"/etc/ssh/libssh_server_config\""),
    );
    cfg.define("HAVE_GETADDRINFO", Some("1"));
    cfg.define("HAVE_LIBCRYPTO", Some("1"));
    cfg.define("HAVE_OPENSSL_AES_H", Some("1"));
    cfg.define("HAVE_OPENSSL_BLOWFISH_H", Some("1"));
    cfg.define("HAVE_OPENSSL_DES_H", Some("1"));
    cfg.define("HAVE_OPENSSL_ECC", Some("1"));
    cfg.define("HAVE_OPENSSL_ECDH_H", Some("1"));
    cfg.define("HAVE_OPENSSL_ECDSA_H", Some("1"));
    cfg.define("HAVE_ECC", Some("1"));
    cfg.define("HAVE_DSA", Some("1"));
    cfg.define("HAVE_OPENSSL_EC_H", Some("1"));

    if openssl_version >= 0x1_01_01_00_0 {
        cfg.define("HAVE_OPENSSL_EVP_CHACHA20", Some("1"));
    }

    /* Don't bother setting these: libssh has a fallback in any case,
     * and the documentation doesn't specify when they were introduced,
    cfg.define("HAVE_OPENSSL_EVP_DIGESTSIGN", Some("1"));
    cfg.define("HAVE_OPENSSL_EVP_DIGESTVERIFY", Some("1"));
    */

    if openssl_version < 0x1_01_00_00_0 {
        cfg.file("vendored/src/libcrypto-compat.c");
    }

    if false && openssl_version >= 0x3_00_00_00_0 {
        cfg.define("HAVE_OPENSSL_EVP_KDF_CTX_NEW_ID", Some("1"));
    }
    // cfg.define("HAVE_OPENSSL_FIPS_MODE", Some("1"));

    cfg.define("HAVE_STDINT_H", Some("1"));
    cfg.define("WITH_ZLIB", Some("1"));
    cfg.define("WITH_GEX", Some("1"));
    cfg.define("WITH_SFTP", Some("1"));
    cfg.define("WITH_SERVER", Some("1"));

    if target.contains("windows") {
        cfg.define("HAVE_IO_H", Some("1"));
        // cfg.define("HAVE_MEMSET_S", Some("1"));
        cfg.define("HAVE_SECURE_ZERO_MEMORY", Some("1"));
        cfg.define("HAVE__SNPRINTF", Some("1"));
        cfg.define("HAVE__SNPRINTF_S", Some("1"));
        cfg.define("HAVE__STRTOUI64", Some("1"));
        cfg.define("HAVE__VSNPRINTF", Some("1"));
        cfg.define("HAVE__VSNPRINTF_S", Some("1"));
        cfg.define("HAVE_ISBLANK", Some("1"));
    } else {
        cfg.define("HAVE_ARPA_INET_H", Some("1"));
        cfg.define("HAVE_CLOCK_GETTIME", Some("1"));
        cfg.define("HAVE_PTHREAD_H", Some("1"));
        cfg.define("HAVE_PTHREAD", Some("1"));
        cfg.define("HAVE_SELECT", Some("1"));
        cfg.define("HAVE_SNPRINTF", Some("1"));
        cfg.define("HAVE_STRTOULL", Some("1"));
        cfg.define("HAVE_SYS_TIME_H", Some("1"));
        cfg.define("HAVE_TERMIOS_H", Some("1"));
        cfg.define("HAVE_UNISTD_H", Some("1"));
        cfg.define("HAVE_VSNPRINTF", Some("1"));

        if !target.contains("darwin") {
            cfg.define("HAVE_POLL", Some("1"));
        }
    }
    if target.contains("linux") {
        cfg.define("HAVE_STRNDUP", Some("1"));
    }
    if target.contains("darwin") {
        cfg.define("HAVE_NTOHLL", Some("1"));
        cfg.define("HAVE_HTONLL", Some("1"));
    }

    if target.contains("android") {
        cfg.define("_BSD_SOURCE", None);
    } else {
        cfg.define("_GNU_SOURCE", None);
    }

    let compiler = cfg.get_compiler();
    if compiler.is_like_gnu() || compiler.is_like_clang() {
        cfg.define("HAVE_COMPILER__FUNCTION__", Some("1"));
        cfg.define("HAVE_COMPILER__FUNC__", Some("1"));
        cfg.define("HAVE_GCC_THREAD_LOCAL_STORAGE", Some("1"));
    }

    if compiler.is_like_msvc() {
        cfg.define("HAVE_COMPILER__FUNC__", Some("1"));
        cfg.define("HAVE_MSC_THREAD_LOCAL_STORAGE", Some("1"));
    }

    std::fs::write(include.join("config.h"), "// nothing").unwrap();

    let version = std::fs::read_to_string("vendored/include/libssh/libssh_version.h.cmake")
        .unwrap()
        .replace("@libssh_VERSION_MAJOR@", "0")
        .replace("@libssh_VERSION_MINOR@", "8")
        .replace("@libssh_VERSION_PATCH@", "90");

    std::fs::create_dir_all(include.join("libssh")).unwrap();
    std::fs::write(include.join("libssh/libssh_version.h"), version).unwrap();

    println!("cargo:rerun-if-env-changed=DEP_Z_INCLUDE");
    if let Some(path) = std::env::var_os("DEP_Z_INCLUDE") {
        cfg.include(path);
    }
    if let Some(zlib_root) = std::env::var_os("DEP_Z_ROOT") {
        println!(
            "cargo:rustc-link-search=native={}",
            PathBuf::from(zlib_root).join("lib").to_str().unwrap()
        );
    }

    println!("cargo:rerun-if-env-changed=DEP_OPENSSL_INCLUDE");
    if let Some(path) = std::env::var_os("DEP_OPENSSL_INCLUDE") {
        if let Some(path) = std::env::split_paths(&path).next() {
            if let Some(path) = path.to_str() {
                if !path.is_empty() {
                    cfg.include(path);
                }
            }
        }
    }
    if let Some(zlib_root) = std::env::var_os("DEP_OPENSSL_ROOT") {
        println!(
            "cargo:rustc-link-search=native={}",
            PathBuf::from(zlib_root).join("lib").to_str().unwrap()
        );
    }

    cfg.warnings(false);
    for f in &[
        "agent.c",
        "auth.c",
        "base64.c",
        "bignum.c",
        "bind_config.c",
        "buffer.c",
        "callbacks.c",
        "chachapoly.c",
        "channels.c",
        "client.c",
        "config.c",
        "config_parser.c",
        "connect.c",
        "connector.c",
        "crypto_common.c",
        "curve25519.c",
        "dh-gex.c",
        "dh.c",
        "dh_crypto.c",
        "ecdh.c",
        "ecdh_crypto.c",
        "error.c",
        "external/bcrypt_pbkdf.c",
        "external/blowfish.c",
        "external/chacha.c",
        "external/curve25519_ref.c",
        "external/ed25519.c",
        "external/fe25519.c",
        "external/ge25519.c",
        "external/poly1305.c",
        "external/sc25519.c",
        "getpass.c",
        "getrandom_crypto.c",
        "gzip.c",
        "init.c",
        "kdf.c",
        "kex.c",
        "known_hosts.c",
        "knownhosts.c",
        "legacy.c",
        "libcrypto.c",
        "log.c",
        "match.c",
        "md_crypto.c",
        "messages.c",
        "misc.c",
        "options.c",
        "packet.c",
        "packet_cb.c",
        "packet_crypt.c",
        "pcap.c",
        "pki.c",
        "pki_container_openssh.c",
        "pki_crypto.c",
        // "pki_ed25519.c", gcrypt only
        "pki_ed25519_common.c",
        "poll.c",
        "scp.c",
        "server.c",
        "session.c",
        "sftp.c",
        "sftpserver.c",
        "sftp_common.c",
        "sftp_aio.c",
        "socket.c",
        "string.c",
        "threads.c",
        "threads/libcrypto.c",
        "threads/noop.c",
        "token.c",
        "ttyopts.c",
        "wrapper.c",
    ] {
        cfg.file(&format!("vendored/src/{}", f));
    }

    if target_family == "unix" {
        cfg.file("vendored/src/threads/pthread.c");
    }
    if target_family == "windows" {
        cfg.file("vendored/src/threads/winlocks.c");
    }
    cfg.compile("libssh");

    if target.contains("windows") {
        println!("cargo:rustc-link-lib=crypt32");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=shell32");
        println!("cargo:rustc-link-lib=ntdll");
        println!("cargo:rustc-link-lib=iphlpapi");
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=advapi32");
    }
    // The non-vendored fallback has no libz-sys dependency to supply this.
    if std::env::var_os("CARGO_FEATURE_VENDORED").is_none() {
        println!("cargo:rustc-link-lib=z");
    }
}
