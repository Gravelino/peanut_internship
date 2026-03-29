fn main() {
    println!("Peanut Internship: Ready for Module 0");
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_invariant() {
        assert!(true);
    }

    #[test]
    #[should_panic(expected = "negative test case")]
    fn test_negative() {
        panic!("negative test case");
    }
}
