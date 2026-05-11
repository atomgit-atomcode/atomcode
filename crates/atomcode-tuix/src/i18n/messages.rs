pub enum Msg<'a> {
    WelcomeBannerLine1,
    ErrUnsupportedLocale { input: &'a str },
}
