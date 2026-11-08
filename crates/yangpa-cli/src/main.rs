//! `yangpa` 실행 파일. 실제 동작은 라이브러리(`yangpa_cli`)에 있습니다.

fn main() {
    std::process::exit(yangpa_cli::main_entry());
}
