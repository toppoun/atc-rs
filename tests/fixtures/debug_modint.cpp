#ifdef ATC_DEBUG_TEST_HEADER_FIRST
#include <atc/debug.hpp>
#endif

#if __has_include(<atcoder/modint>) && !defined(ATC_DEBUG_TEST_MODINT_FIXTURE)
#include <atcoder/modint>
#else
#include <type_traits>

// Match the ACL declarations so the test also runs without an ACL installation.
namespace atcoder {
template <int Modulus, std::enable_if_t<(1 <= Modulus)>* = nullptr>
struct static_modint {
    explicit static_modint(int value) : value_(value % Modulus) {}
    int val() const { return value_; }

private:
    int value_;
};

template <int Id>
struct dynamic_modint {
    static void set_mod(int modulus) { modulus_ = modulus; }
    explicit dynamic_modint(int value) : value_(value % modulus_) {}
    int val() const { return value_; }

private:
    static inline int modulus_;
    int value_;
};

using modint998244353 = static_modint<998244353>;
using modint1000000007 = static_modint<1000000007>;
using modint = dynamic_modint<-1>;
}  // namespace atcoder
#endif

#ifndef ATC_DEBUG_TEST_HEADER_FIRST
#include <atc/debug.hpp>
#endif

struct unregistered_number {
    static int mod() { return 17; }
    int val() const { return 3; }
};

namespace atcoder {
std::ostream& operator<<(std::ostream& output, const static_modint<19>&) {
    return output << "custom";
}
}  // namespace atcoder

int main() {
    using arbitrary_modint = atcoder::dynamic_modint<5>;
    atcoder::modint::set_mod(11);
    arbitrary_modint::set_mod(13);
    const auto alias1 = atcoder::modint998244353(998244355);
    const auto alias2 = atcoder::modint1000000007(1000000011);
    const auto fixed = atcoder::static_modint<17>(20);
    const auto nested = std::tuple{
        std::pair{atcoder::modint(16), arbitrary_modint(20)},
        std::vector{fixed, atcoder::static_modint<17>(0)},
    };

#line 100
    debug(alias1, alias2, fixed, atcoder::modint(16), arbitrary_modint(20));
    debug(nested);
    debug(unregistered_number{}, atcoder::static_modint<19>(1));
}
