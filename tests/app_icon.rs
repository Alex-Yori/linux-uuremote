#[test]
fn app_icon_decodes() {
    let img = linux_uuremote::viewer::png::decode(linux_uuremote::viewer::icons::APP).unwrap();
    assert_eq!((img.width, img.height), (256, 256));
}
