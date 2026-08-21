use crate::error::AppError;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const DEBUG_HPP: &str = include_str!("../assets/debug.hpp");

pub fn materialize_debug_header() -> Result<PathBuf, AppError> {
    let include_dir = crate::paths::debug_include_dir()?;
    materialize_debug_header_at(&include_dir)?;
    Ok(include_dir)
}

#[cfg(test)]
pub(crate) fn materialize_debug_header_in(cache_dir: &Path) -> io::Result<PathBuf> {
    let include_dir = cache_dir.join("include");
    materialize_debug_header_at(&include_dir)?;
    Ok(include_dir)
}

fn materialize_debug_header_at(include_dir: &Path) -> io::Result<()> {
    let cache_dir = include_dir.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "debug include directory has no parent: {}",
                include_dir.display()
            ),
        )
    })?;
    ensure_real_directory(cache_dir, "debug cache directory")?;

    ensure_real_directory(include_dir, "debug include directory")?;

    let atc_dir = include_dir.join("atc");
    ensure_real_directory(&atc_dir, "debug header directory")?;

    let header = atc_dir.join("debug.hpp");
    match fs::symlink_metadata(&header) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "debug header path is not a regular file: {}",
                    header.display()
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Write beside the destination and atomically replace it, so an interrupted update cannot
    // leave a truncated header in the shared cache.
    let mut temporary = tempfile::NamedTempFile::new_in(&atc_dir)?;
    temporary.write_all(DEBUG_HPP.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(&header).map_err(|error| error.error)?;

    Ok(())
}

fn ensure_real_directory(path: &Path, kind: &str) -> io::Result<()> {
    fs::create_dir_all(path)?;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{kind} is not a real directory: {}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn available_cpp_compiler() -> Option<&'static str> {
        ["g++", "clang++"].into_iter().find(|compiler| {
            Command::new(compiler)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    #[test]
    fn materializes_embedded_header_at_expected_cache_path() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("cache with spaces");

        let include_dir = materialize_debug_header_in(&cache_dir).unwrap();
        let header = include_dir.join("atc").join("debug.hpp");

        assert_eq!(include_dir, cache_dir.join("include"));
        assert_eq!(fs::read(&header).unwrap(), DEBUG_HPP.as_bytes());
    }

    #[test]
    fn atomically_replaces_stale_regular_header() {
        let temp = tempfile::tempdir().unwrap();
        let header = temp.path().join("include").join("atc").join("debug.hpp");
        fs::create_dir_all(header.parent().unwrap()).unwrap();
        fs::write(&header, "stale").unwrap();

        materialize_debug_header_in(temp.path()).unwrap();

        assert_eq!(fs::read(header).unwrap(), DEBUG_HPP.as_bytes());
    }

    #[test]
    fn rejects_non_directory_cache_components_and_header_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("include"), "not a directory").unwrap();
        let error = materialize_debug_header_in(temp.path()).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists
                | io::ErrorKind::NotADirectory
                | io::ErrorKind::InvalidInput
        ));

        let second = tempfile::tempdir().unwrap();
        let header = second.path().join("include").join("atc").join("debug.hpp");
        fs::create_dir_all(&header).unwrap();
        let error = materialize_debug_header_in(second.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn embedded_header_preserves_stream_state_and_formats_new_types() {
        let Some(compiler) = available_cpp_compiler() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let include_dir = materialize_debug_header_in(temp.path()).unwrap();
        let source = temp.path().join("debug_regression.cpp");
        let executable = temp.path().join(if cfg!(windows) {
            "debug_regression.exe"
        } else {
            "debug_regression"
        });

        fs::write(
            &source,
            r###"
#ifdef ATC_DEBUG_TEST_NO_INT128
#include <cstddef>
#undef __SIZEOF_INT128__
#endif
#include <atc/debug.hpp>

#include <array>
#include <iomanip>
#include <iostream>
#include <limits>
#include <locale>
#include <sstream>
#include <string>
#include <string_view>
#include <thread>
#include <vector>

enum class signed_enum : long long { value = -7 };
enum class unsigned_enum : unsigned long long {
    value = 18446744073709551615ULL
};

class grouped_punctuation : public std::numpunct<char> {
private:
    char do_thousands_sep() const override { return '_'; }
    std::string do_grouping() const override { return "\3"; }
};

class counting_buffer : public std::stringbuf {
public:
    int sync_count = 0;

private:
    int sync() override {
        ++sync_count;
        return std::stringbuf::sync();
    }
};

class failing_buffer : public std::streambuf {
private:
    std::streamsize xsputn(const char*, std::streamsize) override {
        return 0;
    }

    int_type overflow(int_type = traits_type::eof()) override {
        return traits_type::eof();
    }

    int sync() override { return -1; }
};

class buffer_exception {};

class throwing_buffer : public std::streambuf {
private:
    std::streamsize xsputn(const char*, std::streamsize) override {
        throw buffer_exception{};
    }

    int_type overflow(int_type = traits_type::eof()) override {
        throw buffer_exception{};
    }

    int sync() override { throw buffer_exception{}; }
};

int fail(int code, std::string_view message) {
    std::cout << code << ": " << message << '\n';
    return code;
}

class cerr_restorer {
public:
    cerr_restorer()
        : buffer_(std::cerr.rdbuf()),
          flags_(std::cerr.flags()),
          precision_(std::cerr.precision()),
          width_(std::cerr.width()),
          fill_(std::cerr.fill()),
          locale_(std::cerr.getloc()),
          tie_(std::cerr.tie()),
          state_(std::cerr.rdstate()),
          exceptions_(std::cerr.exceptions()) {}

    ~cerr_restorer() {
        try {
            std::cerr.exceptions(std::ios_base::goodbit);
            std::cerr.clear();
            std::cerr.rdbuf(buffer_);
            std::cerr.flags(flags_);
            std::cerr.precision(precision_);
            std::cerr.width(width_);
            std::cerr.fill(fill_);
            std::cerr.imbue(locale_);
            std::cerr.tie(tie_);
            std::cerr.clear(state_);
            std::cerr.exceptions(exceptions_);
        } catch (...) {
        }
    }

private:
    std::streambuf* buffer_;
    std::ios_base::fmtflags flags_;
    std::streamsize precision_;
    std::streamsize width_;
    char fill_;
    std::locale locale_;
    std::ostream* tie_;
    std::ios_base::iostate state_;
    std::ios_base::iostate exceptions_;
};

int main() {
    std::ostringstream captured;
    cerr_restorer restore_cerr;
    std::cerr.rdbuf(captured.rdbuf());
    std::cerr.exceptions(std::ios_base::goodbit);
    std::cerr.clear();
    std::cerr.flags(std::ios_base::skipws | std::ios_base::dec | std::ios_base::unitbuf);
    std::cerr.precision(6);
    std::cerr.width(0);
    std::cerr.fill(' ');
    std::cerr.imbue(std::locale::classic());
    std::cerr.tie(nullptr);

    const char embedded[] = {'a', '"', '\\', '\0', '\n', '\r', '\t', 'z'};
    std::string string_value(embedded, sizeof(embedded));
    std::string_view view_value(embedded, sizeof(embedded));
    const char* c_string = "c\"\\\n\r\tz";
    const char array_value[] = {'x', '\0', '"', '\\', '\n', '\r', '\t'};
    const char* null_string = nullptr;

    atc_debug::detail::write(
        12,
        "values",
        string_value,
        view_value,
        c_string,
        array_value,
        null_string,
        '\'',
        '\\',
        '\0',
        '\n',
        '\r',
        '\t'
    );

    const std::string expected_strings = R"EXPECTED([L12] values = "a\"\\\0\n\r\tz", "a\"\\\0\n\r\tz", "c\"\\\n\r\tz", "x\0\"\\\n\r\t", <null>, '\'', '\\', '\0', '\n', '\r', '\t'
)EXPECTED";
    if (captured.str() != expected_strings) {
        return fail(1, "escaped string, C-string, array, or character output changed");
    }

    captured.str({});
    captured.clear();
    atc_debug::detail::write(
        13,
        "enums",
        signed_enum::value,
        unsigned_enum::value
    );
    if (
        captured.str()
        != "[L13] enums = -7, 18446744073709551615\n"
    ) {
        return fail(2, "scoped enum output did not use its underlying type");
    }

#ifdef __SIZEOF_INT128__
    captured.str({});
    captured.clear();
    const unsigned __int128 unsigned_max =
        ~static_cast<unsigned __int128>(0);
    const __int128 signed_max = static_cast<__int128>(unsigned_max >> 1);
    const __int128 signed_min = -signed_max - 1;
    atc_debug::detail::write(
        14,
        "integers",
        static_cast<unsigned __int128>(0),
        unsigned_max,
        signed_max,
        signed_min
    );
    if (
        captured.str()
        != "[L14] integers = 0, "
           "340282366920938463463374607431768211455, "
           "170141183460469231731687303715884105727, "
           "-170141183460469231731687303715884105728\n"
    ) {
        return fail(3, "128-bit boundary formatting is incorrect");
    }
#endif

    captured.str({});
    captured.clear();
    counting_buffer tied_buffer;
    std::ostream tied_output(&tied_buffer);
    const std::locale grouped(
        std::locale::classic(),
        new grouped_punctuation
    );
    std::cerr.flags(
        std::ios_base::skipws
        | std::ios_base::hex
        | std::ios_base::showbase
        | std::ios_base::unitbuf
    );
    const auto expected_flags = std::cerr.flags();
    std::cerr.precision(3);
    std::cerr.width(8);
    std::cerr.fill('_');
    std::cerr.imbue(grouped);
    std::cerr.tie(&tied_output);

    atc_debug::detail::write(10, "values", 0x123, 1.23456);
    if (captured.str() != "______[L0xa] values = 0x123, 1.23\n") {
        return fail(4, "flags, precision, width, fill, or locale was not copied");
    }
    if (
        std::cerr.flags() != expected_flags
        || std::cerr.precision() != 3
        || std::cerr.width() != 0
        || std::cerr.fill() != '_'
        || std::cerr.getloc() != grouped
        || std::cerr.tie() != &tied_output
        || tied_buffer.sync_count == 0
    ) {
        return fail(5, "stream formatting state or tied-stream flushing changed");
    }

    std::cerr.tie(nullptr);
    std::cerr.flags(std::ios_base::skipws | std::ios_base::dec | std::ios_base::unitbuf);
    std::cerr.precision(6);
    std::cerr.width(0);
    std::cerr.fill(' ');
    std::cerr.imbue(std::locale::classic());
    captured.str({});
    captured.clear();

    std::cerr.clear(std::ios_base::failbit);
    atc_debug::detail::write(20, "value", 7);
    if (!captured.str().empty()) {
        return fail(6, "debug output ignored the existing cerr failure state");
    }
    std::cerr.clear();

    failing_buffer failure;
    std::cerr.rdbuf(&failure);
    std::cerr.clear();
    std::cerr.exceptions(std::ios_base::badbit);
    bool threw = false;
    try {
        atc_debug::detail::write(21, "value", 7);
    } catch (const std::ios_base::failure&) {
        threw = true;
    }
    const bool recorded_badbit =
        (std::cerr.rdstate() & std::ios_base::badbit) != 0;
    std::cerr.exceptions(std::ios_base::goodbit);
    std::cerr.clear();
    std::cerr.rdbuf(captured.rdbuf());
    if (!threw || !recorded_badbit) {
        return fail(7, "osyncstream emission failure was not propagated to cerr");
    }

    throwing_buffer throwing;
    std::cerr.rdbuf(&throwing);
    std::cerr.clear();
    std::cerr.exceptions(std::ios_base::badbit);
    bool rethrew_buffer_exception = false;
    try {
        atc_debug::detail::write(22, "value", 7);
    } catch (const buffer_exception&) {
        rethrew_buffer_exception = true;
    } catch (...) {
    }
    const bool throwing_buffer_recorded_badbit =
        (std::cerr.rdstate() & std::ios_base::badbit) != 0;
    std::cerr.exceptions(std::ios_base::goodbit);
    std::cerr.clear();
    std::cerr.rdbuf(captured.rdbuf());
    if (!rethrew_buffer_exception || !throwing_buffer_recorded_badbit) {
        return fail(10, "the wrapped stream buffer exception was replaced or lost");
    }

    captured.str({});
    captured.clear();
    constexpr int thread_count = 8;
    constexpr int calls_per_thread = 32;
    constexpr int payload_size = 256;
    std::vector<std::thread> threads;
    for (int index = 0; index < thread_count; ++index) {
        threads.emplace_back([index] {
            const std::string payload(
                payload_size,
                static_cast<char>('A' + index)
            );
            for (int call = 0; call < calls_per_thread; ++call) {
                atc_debug::detail::write(30, "payload", payload);
            }
        });
    }
    for (auto& thread : threads) {
        thread.join();
    }

    std::array<int, thread_count> counts{};
    std::istringstream lines(captured.str());
    std::string line;
    while (std::getline(lines, line)) {
        bool matched = false;
        for (int index = 0; index < thread_count; ++index) {
            const std::string expected =
                "[L30] payload = \""
                + std::string(payload_size, static_cast<char>('A' + index))
                + '"';
            if (line == expected) {
                ++counts[index];
                matched = true;
                break;
            }
        }
        if (!matched) {
            return fail(8, "a concurrent debug line was interleaved");
        }
    }
    for (int count : counts) {
        if (count != calls_per_thread) {
            return fail(9, "a concurrent debug call was lost");
        }
    }

    return 0;
}
"###,
        )
        .unwrap();

        for (suffix, extra_args) in [
            ("native", Vec::<&str>::new()),
            ("without_int128", vec!["-DATC_DEBUG_TEST_NO_INT128"]),
            ("without_syncstream", vec!["-DATC_DEBUG_DISABLE_SYNCSTREAM"]),
        ] {
            let output_path = executable.with_file_name(format!(
                "{}_{}{}",
                executable.file_stem().unwrap().to_string_lossy(),
                suffix,
                executable
                    .extension()
                    .map(|extension| format!(".{}", extension.to_string_lossy()))
                    .unwrap_or_default()
            ));
            let compile = Command::new(compiler)
                .arg("-std=c++23")
                .arg("-Wall")
                .arg("-Wextra")
                .arg("-pthread")
                .args(extra_args)
                .arg("-I")
                .arg(&include_dir)
                .arg(&source)
                .arg("-o")
                .arg(&output_path)
                .output()
                .unwrap();
            assert!(
                compile.status.success(),
                "{compiler} failed to compile {suffix} debug regression:\n{}",
                String::from_utf8_lossy(&compile.stderr)
            );

            let run = Command::new(&output_path).output().unwrap();
            assert!(
                run.status.success(),
                "{suffix} debug regression failed with {:?}:\nstdout:\n{}\nstderr:\n{}",
                run.status.code(),
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
        }
    }
}
