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


int main() {
    ios::sync_with_stdio(false);
    cin.tie(nullptr);
    int n;
    cin >> n;
    bool ans = true;
    for(int i = 0; i < n; i++){
        int x;
        cin >> x;
        if(x >= 0) ans = false;  
    }
    cout << (ans ? "Yes":"No") << endl;
    std::cerr << "Error message1" << std::endl;
    std::cerr << "Error message2" << std::endl;
    std::cerr << "Error message3" << std::endl;
    std::cerr << "Error message4" << std::endl;
    std::cerr << "Error message5" << std::endl;
    std::cerr << "Error message6" << std::endl;
    std::cerr << "Error message7" << std::endl;
    std::cerr << "Error message8" << std::endl;
    std::cerr << "Error message9" << std::endl;
    std::cerr << "Error message10" << std::endl;
    std::cerr << "Error message11" << std::endl;
    std::cerr << "Error message12" << std::endl;
    std::cerr << "Error message13" << std::endl;
    std::cerr << "Error message14" << std::endl;

    return 0;
}
