use std::path::{Path, PathBuf};
use std::{env, fs};

fn new_build() -> cc::Build {
    let mut cfg = cc::Build::new();
    cfg.warnings(false);
    cfg.flag_if_supported("-fno-stack-check");
    cfg.flag_if_supported("-Wno-deprecated-non-prototype");
    cfg
}

fn zlib() {
    require_vendored_sources(&["zlib/adler32.c", "zlib/zlib.h"]);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let mut cfg = new_build();
    let build_dir = out_dir.join("zlib-build");
    fs::create_dir_all(&build_dir).unwrap();
    cfg.out_dir(&build_dir);
    cfg.file("zlib/adler32.c")
        .file("zlib/compress.c")
        .file("zlib/crc32.c")
        .file("zlib/deflate.c")
        .file("zlib/gzclose.c")
        .file("zlib/gzlib.c")
        .file("zlib/gzread.c")
        .file("zlib/gzwrite.c")
        .file("zlib/inflate.c")
        .file("zlib/infback.c")
        .file("zlib/inftrees.c")
        .file("zlib/inffast.c")
        .file("zlib/trees.c")
        .file("zlib/uncompr.c")
        .file("zlib/zutil.c");
    cfg.include("zlib");
    cfg.define("HAVE_SYS_TYPES_H", None);
    cfg.define("HAVE_STDINT_H", None);
    cfg.define("HAVE_STDDEF_H", None);
    let target = env::var("TARGET").unwrap();
    if !target.contains("windows") {
        cfg.define("_LARGEFILE64_SOURCE", Some("1"));
    }
    cfg.compile("z");
}

fn libpng() {
    require_vendored_sources(&["libpng/png.c", "libpng/png.h"]);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let mut cfg = new_build();
    let build_dir = out_dir.join("png-build");
    fs::create_dir_all(&build_dir).unwrap();
    cfg.out_dir(&build_dir);

    cfg.file("libpng/png.c")
        .file("libpng/pngerror.c")
        .file("libpng/pngget.c")
        .file("libpng/pngmem.c")
        .file("libpng/pngpread.c")
        .file("libpng/pngread.c")
        .file("libpng/pngrio.c")
        .file("libpng/pngrtran.c")
        .file("libpng/pngrutil.c")
        .file("libpng/pngset.c")
        .file("libpng/pngtrans.c")
        .file("libpng/pngwio.c")
        .file("libpng/pngwrite.c")
        .file("libpng/pngwtran.c")
        .file("libpng/pngwutil.c");

    if let Ok(arch) = env::var("CARGO_CFG_TARGET_ARCH") {
        match arch.as_str() {
            "aarch64" | "arm" => {
                cfg.file("libpng/arm/arm_init.c")
                    .file("libpng/arm/filter_neon.S")
                    .file("libpng/arm/filter_neon_intrinsics.c")
                    .file("libpng/arm/palette_neon_intrinsics.c");
            }
            _ => {}
        }
    }

    cfg.include("zlib");
    cfg.include("libpng");
    cfg.include(&build_dir);
    cfg.define("HAVE_SYS_TYPES_H", None);
    cfg.define("HAVE_STDINT_H", None);
    cfg.define("HAVE_STDDEF_H", None);
    let target = env::var("TARGET").unwrap();
    if target.contains("powerpc64") {
        cfg.define("PNG_POWERPC_VSX_OPT", Some("0"));
    }
    if !target.contains("windows") {
        cfg.define("_LARGEFILE64_SOURCE", Some("1"));
    }

    fs::write(
        build_dir.join("pnglibconf.h"),
        fs::read_to_string("libpng/scripts/pnglibconf.h.prebuilt").unwrap(),
    )
    .unwrap();

    cfg.compile("png");
}

fn freetype() {
    require_vendored_sources(&[
        "freetype2/src/base/ftbase.c",
        "freetype2/include/ft2build.h",
    ]);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let mut cfg = new_build();
    let build_dir = out_dir.join("freetype-build");
    fs::create_dir_all(&build_dir).unwrap();
    cfg.out_dir(&build_dir);
    cfg.include("zlib");
    cfg.include("libpng");
    cfg.include(out_dir.join("png-build"));

    // Cargo can reuse these native artifacts after the source checkout moves.
    // Export a complete header tree owned by OUT_DIR, including the configured
    // ftoption.h below, rather than a path into a transient source checkout.
    let include_dir = build_dir.join("freetype2/include");
    copy_include_tree(Path::new("freetype2/include"), &include_dir)
        .expect("copy vendored FreeType headers into OUT_DIR");
    cfg.include(&include_dir);
    cfg.define("FT2_BUILD_LIBRARY", None);

    let target = env::var("TARGET").unwrap();

    fs::write(
        build_dir.join("freetype2/include/freetype/config/ftoption.h"),
        fs::read_to_string("freetype2/include/freetype/config/ftoption.h")
            .unwrap()
            .replace(
                "/* #define FT_CONFIG_OPTION_ERROR_STRINGS */",
                "#define FT_CONFIG_OPTION_ERROR_STRINGS",
            )
            .replace(
                "/* #define FT_CONFIG_OPTION_SYSTEM_ZLIB */",
                "#define FT_CONFIG_OPTION_SYSTEM_ZLIB",
            )
            .replace(
                "/* #define FT_CONFIG_OPTION_USE_PNG */",
                "#define FT_CONFIG_OPTION_USE_PNG",
            )
            .replace(
                "#define TT_CONFIG_OPTION_SUBPIXEL_HINTING  2",
                "#define TT_CONFIG_OPTION_SUBPIXEL_HINTING  3",
            )
            .replace(
                "/* #define PCF_CONFIG_OPTION_LONG_FAMILY_NAMES */",
                "#define PCF_CONFIG_OPTION_LONG_FAMILY_NAMES",
            )
            .replace(
                "/* #define FT_CONFIG_OPTION_SUBPIXEL_RENDERING */",
                "#define FT_CONFIG_OPTION_SUBPIXEL_RENDERING",
            ),
    )
    .unwrap();

    for f in [
        "autofit/autofit.c",
        "base/ftbase.c",
        "base/ftbbox.c",
        "base/ftbdf.c",
        "base/ftbitmap.c",
        "base/ftcid.c",
        "base/ftfstype.c",
        "base/ftgasp.c",
        "base/ftglyph.c",
        "base/ftgxval.c",
        "base/ftinit.c",
        "base/ftmm.c",
        "base/ftotval.c",
        "base/ftpatent.c",
        "base/ftpfr.c",
        "base/ftstroke.c",
        "base/ftsynth.c",
        "base/ftsystem.c",
        "base/fttype1.c",
        "base/ftwinfnt.c",
        "bdf/bdf.c",
        "bzip2/ftbzip2.c",
        "cache/ftcache.c",
        "cff/cff.c",
        "cid/type1cid.c",
        "gzip/ftgzip.c",
        "lzw/ftlzw.c",
        "pcf/pcf.c",
        "pfr/pfr.c",
        "psaux/psaux.c",
        "pshinter/pshinter.c",
        "psnames/psnames.c",
        "raster/raster.c",
        "sdf/ftbsdf.c",
        "sdf/ftsdf.c",
        "sdf/ftsdfcommon.c",
        "sdf/ftsdfrend.c",
        "sfnt/sfnt.c",
        "smooth/smooth.c",
        "svg/ftsvg.c",
        "truetype/truetype.c",
        "type1/type1.c",
        "type42/type42.c",
        "winfonts/winfnt.c",
    ]
    .iter()
    {
        cfg.file(format!("freetype2/src/{}", f));
    }

    if target.contains("windows") {
        cfg.file("freetype2/builds/windows/ftdebug.c");
    } else {
        cfg.file("freetype2/src/base/ftdebug.c");
    }

    cfg.compile("freetype");

    // These cause DEP_FREETYPE_INCLUDE and DEP_FREETYPE_LIB to be
    // defined in the harfbuzz/build.rs
    println!("cargo:include={}", include_dir.display());
    println!("cargo:lib={}", build_dir.display());
}

fn require_vendored_sources(paths: &[&str]) {
    for path in paths {
        assert!(
            Path::new(path).is_file(),
            "missing vendored source {}; initialize the vendored sources before building",
            path
        );
    }
}

fn copy_include_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_include_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unsupported vendored header entry: {}",
                    entry.path().display()
                ),
            ));
        }
    }
    Ok(())
}

fn main() {
    for source in ["zlib", "libpng", "freetype2"] {
        println!("cargo:rerun-if-changed={}", source);
    }
    zlib();
    libpng();
    freetype();
    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:outdir={}", out_dir);
    println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.12");
}
