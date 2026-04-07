pub fn fizzbuzz(n: u32) -> String {
    match (n % 3, n % 5) {
        (0, 0) => "FizzBuzz".to_string(),
        (0, _) => "Fizz".to_string(),
        (_, 0) => "Buzz".to_string(),
        _ => n.to_string(),
    }
}

pub fn is_palindrome(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    for i in 0..n / 2 {
        if chars[i] != chars[n - 1 - i] {
            return false;
        }
    }
    true
}

pub fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}
