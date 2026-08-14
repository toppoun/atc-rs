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
    for(int i = 0; i < 1e7; i++){
        cout << i << " ";
    }
    for(int i = 0; i < 1e7; i++){
        cout << i << "\n";
    }
    cout << (ans ? "Yes":"No") << endl;
    debug(ans);

    return 0;
}
