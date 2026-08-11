use crate::atcoder;
use crate::error::AppError;
use crate::workspace;

const TEMPLATE: &str = r#"#include <bits/stdc++.h>
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

    return 0;
}
"#;

pub fn new(contest_id: String) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = cwd.join(&contest_id);

    if destination.exists() {
        return Ok(());
    }

    let atcoder = if let Ok(path) = std::env::var("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)?
    } else {
        atcoder::AtCoderClient::new()?
    };

    let contest = atcoder.fetch_contest(&contest_id)?;

    workspace::create_contest_dir(&destination)?;

    workspace::save_metadata(&destination, &contest)?;

    workspace::create_source_files(&destination, &contest.problems, TEMPLATE)?;

    for problem in &contest.problems {
        match atcoder.fetch_samples(problem) {
            Ok(samples) => {
                workspace::save_samples(&destination, problem, &samples)?;
            }

            Err(err) => {
                eprintln!(
                    "[WARN] failed to fetch samples for {}: {err:#?}",
                    problem.index
                );
            }
        }
    }

    Ok(())
}
