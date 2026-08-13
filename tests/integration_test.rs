use bili_add_on::interaction::Args;
use clap::Parser;

#[test]
fn test_cli_full_args_parsing() {
    let args = Args::try_parse_from([
        "bili_add_on",
        "--input",
        "video.mp4",
        "--output",
        "out.mp4",
        "--bvid",
        "BV1fRNH6kEra",
        "--opacity",
        "0.5",
        "--top-ratio",
        "0.1",
        "--bottom-ratio",
        "0.9",
        "--font-scale",
        "1.5",
        "--speed",
        "5",
        "--line-spacing",
        "3",
        "--fixed-duration",
        "10.0",
        "--encoder",
        "software",
        "--quiet",
    ])
    .unwrap();

    assert_eq!(args.input, "video.mp4");
    assert_eq!(args.output.unwrap(), "out.mp4");
    assert_eq!(args.source.bvid.unwrap(), "BV1fRNH6kEra");
    assert!(args.source.xml.is_none());
    assert!((args.opacity - 0.5).abs() < f64::EPSILON);
    assert!((args.top_ratio - 0.1).abs() < f64::EPSILON);
    assert!((args.bottom_ratio - 0.9).abs() < f64::EPSILON);
    assert_eq!(args.font_scale, 1.5);
    assert_eq!(args.speed, 5);
    assert_eq!(args.line_spacing, 3);
    assert!((args.fixed_duration - 10.0).abs() < f64::EPSILON);
    assert_eq!(args.encoder, "software");
    assert!(args.quiet);
    assert!(!args.no_audio);
}

#[test]
fn test_cli_default_values() {
    let args =
        Args::try_parse_from(["bili_add_on", "--input", "video.mp4", "--bvid", "BV1test"]).unwrap();

    assert!((args.opacity - 0.93).abs() < f64::EPSILON);
    assert!((args.top_ratio - 0.0).abs() < f64::EPSILON);
    assert!((args.bottom_ratio - 1.0).abs() < f64::EPSILON);
    assert!((args.font_scale - 1.0).abs() < f32::EPSILON);
    assert_eq!(args.speed, 3);
    assert_eq!(args.line_spacing, 4);
    assert!((args.fixed_duration - 5.0).abs() < f64::EPSILON);
    assert_eq!(args.encoder, "auto");
    assert!(!args.quiet);
    assert!(!args.no_audio);
    assert!(args.output.is_none());
}

#[test]
fn test_cli_xml_source() {
    let args = Args::try_parse_from([
        "bili_add_on",
        "--input",
        "video.mp4",
        "--xml",
        "danmaku.xml",
    ])
    .unwrap();

    assert_eq!(args.source.xml.unwrap().to_string_lossy(), "danmaku.xml");
    assert!(args.source.bvid.is_none());
}

#[test]
fn test_cli_encoder_options() {
    for enc in &["auto", "nvenc", "amf", "qsv", "software"] {
        let args = Args::try_parse_from([
            "bili_add_on",
            "--input",
            "v.mp4",
            "--bvid",
            "BV1test",
            "--encoder",
            enc,
        ])
        .unwrap();
        assert_eq!(args.encoder, *enc);
    }
}

#[test]
fn test_check_output_path_generation() {
    let mut args = Args::try_parse_from([
        "bili_add_on",
        "--input",
        "/home/user/videos/test_video.mp4",
        "--bvid",
        "BV1test",
    ])
    .unwrap();

    args.check_output().unwrap();
    let out = args.output.unwrap();
    assert_eq!(
        std::path::Path::new(&out)
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "bili_add_on_test_video.mp4"
    );
}

#[test]
fn test_cli_stdin_stdout_special_values() {
    let args = Args::try_parse_from([
        "bili_add_on",
        "--input",
        bili_add_on::interaction::STDIN,
        "--output",
        bili_add_on::interaction::STDOUT,
        "--bvid",
        "BV1test",
    ])
    .unwrap();
    assert_eq!(args.input, bili_add_on::interaction::STDIN);
    assert_eq!(args.output.as_deref(), Some(bili_add_on::interaction::STDOUT));
}

#[test]
fn test_cli_device_input() {
    let args = Args::try_parse_from([
        "bili_add_on",
        "--input",
        bili_add_on::interaction::DEVICE,
        "--output",
        "out.mp4",
        "--capture",
        "gdigrab:desktop",
        "--range",
        "30",
        "--bvid",
        "BV1test",
    ])
    .unwrap();
    assert_eq!(args.input, bili_add_on::interaction::DEVICE);
    assert_eq!(args.capture.as_deref(), Some("gdigrab:desktop"));
    assert_eq!(args.range.as_deref(), Some("30"));
}

#[test]
fn test_cli_parse_fails_missing_source() {
    let args = Args::try_parse_from(["bili_add_on", "--input", "video.mp4"]);
    assert!(args.is_err());
}

#[test]
fn test_cli_parse_fails_missing_input() {
    let args = Args::try_parse_from(["bili_add_on", "--bvid", "BV1test"]);
    assert!(args.is_err());
}

#[test]
fn test_danmaku_xml_format_compatibility() {
    // Verify B站's actual XML structure can be parsed correctly.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
    <chatserver>chat.bilibili.com</chatserver>
    <chatid>17001</chatid>
    <mission>0</mission>
    <maxlimit>3000</maxlimit>
    <state>0</state>
    <real_name>0</real_name>
    <source>e-r</source>
    <d p="0.95400,1,25,16777215,1738389256,0,057b89f9,115610398131421632">{}</d>
    <d p="1.53600,4,18,65280,1738389257,0,09ace8a8,115610425619205632">{}</d>
    <d p="2.37500,5,25,255,1738389258,0,d332cc0f,115610230595687936">{}</d>
    <d p="3.77400,6,25,16711680,1738389259,0,0193b302,115610329733865984">{}</d>
    <d p="7.41500,9,25,13684944,1738389260,0,0102df42,115610320960305664">{}</d>
</i>"#,
        "普通滚动弹幕", "底部弹幕", "顶部弹幕", "逆向弹幕", "BAS弹幕",
    );

    let danmakus = bili_add_on::danmaku::parse_danmakus(xml).unwrap();
    assert_eq!(danmakus.len(), 5);

    use bili_add_on::danmaku::DanmakuMode;
    assert!(matches!(danmakus[0].mode, DanmakuMode::Scroll));
    assert!(matches!(danmakus[1].mode, DanmakuMode::Bottom));
    assert!(matches!(danmakus[2].mode, DanmakuMode::Top));
    assert!(matches!(danmakus[3].mode, DanmakuMode::Reverse));
    assert!(matches!(danmakus[4].mode, DanmakuMode::Bas));

    assert_eq!(danmakus[0].text, "普通滚动弹幕");
    assert_eq!(danmakus[1].text, "底部弹幕");
    assert_eq!(danmakus[2].text, "顶部弹幕");
    assert_eq!(danmakus[3].text, "逆向弹幕");
    assert_eq!(danmakus[4].text, "BAS弹幕");
}
