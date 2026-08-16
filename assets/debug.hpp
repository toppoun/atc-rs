#pragma once

// SPDX-License-Identifier: MIT
//
// Based on Heltion/debug.h.
// Upstream commit: d19de7037bd8e2c0a66960635f488e8aeced1bd1.
// Copyright (c) 2023 Yaowei Lyu.
// Modified for the atc-rs competitive-programming CLI.
//
// Full license text: assets/licenses/Heltion-debug.h-MIT.txt.

#include <concepts>
#include <cstddef>
#include <iostream>
#include <queue>
#include <ranges>
#include <stack>
#include <string>
#include <string_view>
#include <syncstream>
#include <tuple>
#include <type_traits>
#include <utility>
#include <vector>

namespace atc_debug {
namespace detail {

template <class T>
using remove_cvref_t = std::remove_cv_t<std::remove_reference_t<T>>;

template <class T>
using const_value_t = std::add_const_t<remove_cvref_t<T>>;

template <class T>
inline constexpr bool is_char_pointer_v =
    std::is_pointer_v<remove_cvref_t<T>>
    && std::is_same_v<
        std::remove_cv_t<std::remove_pointer_t<remove_cvref_t<T>>>,
        char
    >;

template <class T>
inline constexpr bool is_char_array_v =
    std::is_array_v<remove_cvref_t<T>>
    && std::is_same_v<
        std::remove_cv_t<std::remove_extent_t<remove_cvref_t<T>>>,
        char
    >;

template <class T>
concept StringLike =
    is_char_pointer_v<T>
    || is_char_array_v<T>
    || std::convertible_to<const remove_cvref_t<T>&, std::string_view>;

template <class T>
struct is_pair : std::false_type {};

template <class First, class Second>
struct is_pair<std::pair<First, Second>> : std::true_type {};

template <class T>
inline constexpr bool is_pair_v = is_pair<remove_cvref_t<T>>::value;

template <class T, class = void>
struct is_tuple_like : std::false_type {};

template <class T>
struct is_tuple_like<
    T,
    std::void_t<decltype(std::tuple_size<remove_cvref_t<T>>::value)>
> : std::true_type {};

template <class T>
inline constexpr bool is_tuple_like_v = is_tuple_like<T>::value;

template <class T>
struct is_queue : std::false_type {};

template <class Value, class Container>
struct is_queue<std::queue<Value, Container>> : std::true_type {};

template <class T>
inline constexpr bool is_queue_v = is_queue<remove_cvref_t<T>>::value;

template <class T>
struct is_stack : std::false_type {};

template <class Value, class Container>
struct is_stack<std::stack<Value, Container>> : std::true_type {};

template <class T>
inline constexpr bool is_stack_v = is_stack<remove_cvref_t<T>>::value;

template <class T>
struct is_priority_queue : std::false_type {};

template <class Value, class Container, class Compare>
struct is_priority_queue<
    std::priority_queue<Value, Container, Compare>
> : std::true_type {};

template <class T>
inline constexpr bool is_priority_queue_v =
    is_priority_queue<remove_cvref_t<T>>::value;

template <class T>
struct is_vector_bool : std::false_type {};

template <class Allocator>
struct is_vector_bool<std::vector<bool, Allocator>> : std::true_type {};

template <class T>
inline constexpr bool is_vector_bool_v =
    is_vector_bool<remove_cvref_t<T>>::value;

template <class T>
concept Iterable = std::ranges::range<const_value_t<T>>;

template <class T>
concept Streamable = requires(
    std::ostream& output,
    const remove_cvref_t<T>& value
) {
    output << value;
};

inline void copy_format_state(
    std::ostream& destination,
    const std::ostream& source
) {
    destination.flags(source.flags());
    destination.precision(source.precision());
    destination.width(source.width());
    destination.fill(source.fill());
    destination.imbue(source.getloc());
}

inline bool needs_escape(char value, char quote) {
    return value == '\\'
        || value == '\n'
        || value == '\r'
        || value == '\t'
        || value == '\0'
        || value == quote;
}

inline void print_escaped_char(
    std::ostream& output,
    char value,
    char quote
) {
    switch (value) {
        case '\\':
            output << "\\\\";
            break;
        case '\n':
            output << "\\n";
            break;
        case '\r':
            output << "\\r";
            break;
        case '\t':
            output << "\\t";
            break;
        case '\0':
            output << "\\0";
            break;
        default:
            if (value == quote) {
                output << '\\';
            }
            output << value;
            break;
    }
}

inline void print_quoted_string(
    std::ostream& output,
    std::string_view value
) {
    output << '"';

    std::size_t chunk_begin = 0;

    for (std::size_t index = 0; index < value.size(); ++index) {
        if (!needs_escape(value[index], '"')) {
            continue;
        }

        if (chunk_begin < index) {
            output.write(
                value.data() + chunk_begin,
                static_cast<std::streamsize>(index - chunk_begin)
            );
        }

        print_escaped_char(output, value[index], '"');
        chunk_begin = index + 1;
    }

    if (chunk_begin < value.size()) {
        output.write(
            value.data() + chunk_begin,
            static_cast<std::streamsize>(value.size() - chunk_begin)
        );
    }

    output << '"';
}

inline void print_quoted_c_string(
    std::ostream& output,
    const char* value
) {
    output << '"';

    const char* chunk_begin = value;
    const char* current = value;

    while (*current != '\0') {
        if (!needs_escape(*current, '"')) {
            ++current;
            continue;
        }

        if (chunk_begin != current) {
            output.write(
                chunk_begin,
                static_cast<std::streamsize>(current - chunk_begin)
            );
        }

        print_escaped_char(output, *current, '"');

        ++current;
        chunk_begin = current;
    }

    if (chunk_begin != current) {
        output.write(
            chunk_begin,
            static_cast<std::streamsize>(current - chunk_begin)
        );
    }

    output << '"';
}

#ifdef __SIZEOF_INT128__
inline void print_uint128(
    std::ostream& output,
    unsigned __int128 value
) {
    char buffer[40];
    char* end = buffer + sizeof(buffer);
    char* current = end;

    do {
        *--current = static_cast<char>('0' + value % 10);
        value /= 10;
    } while (value != 0);

    output.write(
        current,
        static_cast<std::streamsize>(end - current)
    );
}

inline void print_int128(
    std::ostream& output,
    __int128 value
) {
    if (value < 0) {
        output.put('-');

        unsigned __int128 magnitude =
            static_cast<unsigned __int128>(-(value + 1)) + 1;

        print_uint128(output, magnitude);
    } else {
        print_uint128(
            output,
            static_cast<unsigned __int128>(value)
        );
    }
}
#endif

template <class T>
void print_string_like(std::ostream& output, const T& value) {
    using Value = remove_cvref_t<T>;

    if constexpr (is_char_pointer_v<Value>) {
        if (value == nullptr) {
            output << "<null>";
            return;
        }
        print_quoted_c_string(output, value);
    } else if constexpr (is_char_array_v<Value>) {
        constexpr std::size_t extent = std::extent_v<Value>;
        std::size_t size = extent;
        if (size > 0 && value[size - 1] == '\0') {
            --size;
        }
        print_quoted_string(output, std::string_view(value, size));
    } else {
        print_quoted_string(output, std::string_view(value));
    }
}

template <class T>
void print_value(std::ostream& output, const T& value);

template <class T>
void print_iterable(std::ostream& output, const T& value) {
    output << '{';
    bool first = true;
    for (const auto& element : value) {
        if (!first) {
            output << ", ";
        }
        first = false;
        if constexpr (is_vector_bool_v<T>) {
            output << (element ? '1' : '0');
        } else {
            print_value(output, element);
        }
    }
    output << '}';
}

template <class T>
void print_tuple(std::ostream& output, const T& value) {
    output << '(';
    bool first = true;
    std::apply(
        [&](const auto&... elements) {
            (
                (
                    output << (first ? "" : ", "),
                    first = false,
                    print_value(output, elements)
                ),
                ...
            );
        },
        value
    );
    output << ')';
}

template <class T>
void print_adapter(std::ostream& output, const T& value) {
    auto copy = value;
    output << '{';
    bool first = true;

    while (!copy.empty()) {
        if (!first) {
            output << ", ";
        }
        first = false;

        if constexpr (is_queue_v<T>) {
            print_value(output, copy.front());
        } else {
            print_value(output, copy.top());
        }
        copy.pop();
    }

    output << '}';
}

template <class T>
void print_value(std::ostream& output, const T& value) {
    using Value = remove_cvref_t<T>;

    if constexpr (std::is_same_v<Value, bool>) {
        output << (value ? "true" : "false");
    } else if constexpr (std::is_same_v<Value, char>) {
        output << '\'';
        print_escaped_char(output, value, '\'');
        output << '\'';
#ifdef __SIZEOF_INT128__
    } else if constexpr (std::is_same_v<Value, __int128>) {
        print_int128(output, value);
    } else if constexpr (std::is_same_v<Value, unsigned __int128>) {
        print_uint128(output, value);
#endif
    } else if constexpr (std::is_enum_v<Value>) {
        print_value(
            output,
            static_cast<std::underlying_type_t<Value>>(value)
        );
    } else if constexpr (StringLike<T>) {
        print_string_like(output, value);
    } else if constexpr (is_pair_v<T>) {
        output << '(';
        print_value(output, value.first);
        output << ", ";
        print_value(output, value.second);
        output << ')';
    } else if constexpr (Iterable<T>) {
        print_iterable(output, value);
    } else if constexpr (is_tuple_like_v<T>) {
        print_tuple(output, value);
    } else if constexpr (
        is_queue_v<T>
        || is_stack_v<T>
        || is_priority_queue_v<T>
    ) {
        if constexpr (std::is_copy_constructible_v<Value>) {
            print_adapter(output, value);
        } else {
            output << "<unprintable>";
        }
    } else if constexpr (Streamable<T>) {
        output << value;
    } else {
        output << "<unprintable>";
    }
}

inline void write(std::size_t line, std::string_view) {
    std::osyncstream output(std::cerr);
    copy_format_state(output, std::cerr);

    output << "[L" << line << "]\n";

    copy_format_state(std::cerr, output);
}

template <class... Values>
void write(
    std::size_t line,
    std::string_view expressions,
    const Values&... values
) {
    std::osyncstream output(std::cerr);
    copy_format_state(output, std::cerr);

    output << "[L" << line << "] " << expressions << " = ";
    bool first = true;
    (
        (
            output << (first ? "" : ", "),
            first = false,
            print_value(output, values)
        ),
        ...
    );
    output << '\n';

    copy_format_state(std::cerr, output);
}

}  // namespace detail
}  // namespace atc_debug

#define debug(...) \
    ::atc_debug::detail::write( \
        __LINE__, \
        #__VA_ARGS__ __VA_OPT__(,) __VA_ARGS__ \
    )