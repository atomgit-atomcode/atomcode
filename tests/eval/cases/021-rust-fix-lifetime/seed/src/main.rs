// This file does not compile. Fix the lifetime annotation on longest_word
// and the borrow-after-move bug in main(). Do not change the function's
// parameter count or return type structure — only annotate lifetimes.

fn longest_word(sentence: &str) -> &str {
    let mut best = "";
    for w in sentence.split_whitespace() {
        if w.len() > best.len() {
            best = w;
        }
    }
    best
}

fn main() {
    let s = String::from("simple steady refactoring wins");
    let longest = longest_word(&s);
    drop(s); // BUG: s is moved/dropped but longest still borrows from it
    println!("{}", longest);
}
