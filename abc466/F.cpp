#include <bits/stdc++.h>
using namespace std;

#ifdef LOCAL
#include <atc/debug.hpp>
#else
#define debug(...) ((void)0)
#endif

using ll = long long;

// 1. 見方・状態:
// 
// 2. 答えに必要な情報:
// 
// 3. 捨てる情報と根拠:
// 
// 4. 初期化・更新・判定・計算量:
// 

struct Point {
    int x;
    int y;
};

ostream& operator<<(ostream& os, const Point& p) {
    return os << "Point(" << p.x << ", " << p.y << ")";
}

int main() {
    // ============================================================
    // Basic
    // ============================================================

    debug();
    debug(42);
    debug(true);
    debug(false);
    debug('a');

    // ============================================================
    // Multiple arguments
    // ============================================================

    int x = 10;
    int y = 20;

    debug(x, y);
    debug(x, y, x + y);

    // ============================================================
    // Strings / escaping
    // ============================================================

    string normal = "hello";
    string escaped = "quote:\" slash:\\ newline:\n tab:\t";
    string with_nul{"a\0b", 3};

    debug(normal);
    debug(escaped);
    debug(with_nul);

    char quote = '\'';
    char backslash = '\\';
    char newline = '\n';
    char tab = '\t';
    char nul = '\0';

    debug(quote);
    debug(backslash);
    debug(newline);
    debug(tab);
    debug(nul);

    // ============================================================
    // char pointer
    // ============================================================

    const char* cstr = "hello";
    const char* null_cstr = nullptr;

    debug(cstr);
    debug(null_cstr);

    // Embedded NUL in char array.
    char chars[] = {'a', '\0', 'b', '\0'};
    debug(chars);

    // ============================================================
    // Addresses / pointers
    // ============================================================

    int value = 123;
    int* ptr = &value;
    int* null_ptr = nullptr;

    debug(ptr);
    debug(null_ptr);

    // ============================================================
    // Empty containers
    // ============================================================

    vector<int> empty_vector;
    set<int> empty_set;
    map<int, int> empty_map;
    queue<int> empty_queue;
    stack<int> empty_stack;
    priority_queue<int> empty_priority_queue;

    debug(empty_vector);
    debug(empty_set);
    debug(empty_map);
    debug(empty_queue);
    debug(empty_stack);
    debug(empty_priority_queue);

    // ============================================================
    // Nested containers
    // ============================================================

    vector<vector<int>> vv = {
        {1, 2},
        {},
        {3, 4, 5},
    };

    map<string, vector<int>> mv = {
        {"a", {1, 2}},
        {"b", {3, 4}},
    };

    debug(vv);
    debug(mv);

    // ============================================================
    // pair / tuple
    // ============================================================

    pair<int, string> p = {1, "hello"};
    tuple<int, string, bool> t = {42, "abc", true};

    debug(p);
    debug(t);

    // ============================================================
    // vector<bool>
    // ============================================================

    vector<bool> vb = {true, false, true};

    debug(vb);

    // ============================================================
    // Container adapters
    // ============================================================

    queue<int> q;
    q.push(1);
    q.push(2);
    q.push(3);

    stack<int> st;
    st.push(1);
    st.push(2);
    st.push(3);

    priority_queue<int> pq;
    pq.push(1);
    pq.push(3);
    pq.push(2);

    debug(q);
    debug(st);
    debug(pq);

    // ============================================================
    // User-defined operator<<
    // ============================================================

    Point point{3, 7};

    debug(point);

    // ============================================================
    // Mixed
    // ============================================================

    debug(x, normal, vv, p, point);

    cerr << hex;

    int number = 255;
    debug(number);


    cerr << dec;
    cerr << fixed << setprecision(3);

    double pi = 3.1415926535;
    debug(pi);

    cerr << defaultfloat;

    __int128 i128 = -1234567890123456789LL;
    unsigned __int128 u128 = 12345678901234567890ULL;

    enum class State {
        Unvisited,
        Visiting,
        Done,
    };

    State state = State::Done;

    debug(i128);
    debug(u128);
    debug(state);

    __int128 i128_min =
    -((static_cast<__int128>(1) << 127) - 1) - 1;

    debug(i128_min);

    // string huge(1'000'000, 'a');
    // debug(huge);

    // string huge_escaped;
    // for (int i = 0; i < 200000; ++i) {
    //     huge_escaped += "abcd\n";
    // }
    // debug(huge_escaped);

    // vector<int> a(1e5, 1);
    // debug(a);
    return 0;
}