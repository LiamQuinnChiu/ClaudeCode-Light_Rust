fn main() {
    // esp-hal 提供的链接脚本（RISC-V）
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    // 链接错误时给出针对性的提示（esp-hal 模板同款）
    println!(
        "cargo:rustc-link-arg=--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
