use bili_add_on::interaction::{Cli, Commands};
use clap::Parser;

#[test]
fn test_cli_full_args_parsing() {
    let cli = Cli::try_parse_from([
        "bili_add_on",
        "overlay",
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

    let Commands::Overlay(args) = cli.command else {
        panic!("expected overlay");
    };
    assert_eq!(args.input, "video.mp4");
    assert_eq!(args.output.as_deref(), Some("out.mp4"));
    assert_eq!(args.render.source.bvid.unwrap(), "BV1fRNH6kEra");
    assert!(args.render.source.xml.is_none());
    assert!((args.render.opacity - 0.5).abs() < f64::EPSILON);
    assert!((args.render.top_ratio - 0.1).abs() < f64::EPSILON);
    assert!((args.render.bottom_ratio - 0.9).abs() < f64::EPSILON);
    assert_eq!(args.render.font_scale, 1.5);
    assert_eq!(args.render.speed, 5);
    assert_eq!(args.render.line_spacing, 3);
    assert!((args.render.fixed_duration - 10.0).abs() < f64::EPSILON);
    assert_eq!(args.render.encoder, "software");
    assert!(args.render.quiet);
    assert!(!args.render.no_audio);
}

#[test]
fn test_cli_default_values() {
    let cli =
        Cli::try_parse_from(["bili_add_on", "overlay", "--input", "video.mp4", "--bvid", "BV1test"])
            .unwrap();

    let Commands::Overlay(args) = cli.command else {
        panic!("expected overlay");
    };
    assert!((args.render.opacity - 0.93).abs() < f64::EPSILON);
    assert!((args.render.top_ratio - 0.0).abs() < f64::EPSILON);
    assert!((args.render.bottom_ratio - 1.0).abs() < f64::EPSILON);
    assert!((args.render.font_scale - 1.0).abs() < f32::EPSILON);
    assert_eq!(args.render.speed, 3);
    assert_eq!(args.render.line_spacing, 4);
    assert!((args.render.fixed_duration - 5.0).abs() < f64::EPSILON);
    assert_eq!(args.render.encoder, "auto");
    assert!(!args.render.quiet);
    assert!(!args.render.no_audio);
    assert!(args.output.is_none());
}

#[test]
fn test_cli_xml_source() {
    let cli = Cli::try_parse_from([
        "bili_add_on",
        "overlay",
        "--input",
        "video.mp4",
        "--xml",
        "danmaku.xml",
    ])
    .unwrap();

    let Commands::Overlay(args) = cli.command else {
        panic!("expected overlay");
    };
    assert_eq!(args.render.source.xml.unwrap().to_string_lossy(), "danmaku.xml");
    assert!(args.render.source.bvid.is_none());
}

#[test]
fn test_cli_encoder_options() {
    for enc in &["auto", "nvenc", "amf", "qsv", "software"] {
        let cli = Cli::try_parse_from([
            "bili_add_on",
            "overlay",
            "--input",
            "v.mp4",
            "--bvid",
            "BV1test",
            "--encoder",
            enc,
        ])
        .unwrap();
        let Commands::Overlay(args) = cli.command else {
            panic!("expected overlay");
        };
        assert_eq!(args.render.encoder, *enc);
    }
}

#[test]
fn test_check_output_path_generation() {
    let cli = Cli::try_parse_from([
        "bili_add_on",
        "overlay",
        "--input",
        "/home/user/videos/test_video.mp4",
        "--bvid",
        "BV1test",
    ])
    .unwrap();

    let Commands::Overlay(mut args) = cli.command else {
        panic!("expected overlay");
    };
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
    let cli = Cli::try_parse_from([
        "bili_add_on",
        "overlay",
        "--input",
        bili_add_on::interaction::STDIN,
        "--output",
        bili_add_on::interaction::STDOUT,
        "--bvid",
        "BV1test",
    ])
    .unwrap();

    let Commands::Overlay(args) = cli.command else {
        panic!("expected overlay");
    };
    assert_eq!(args.input, bili_add_on::interaction::STDIN);
    assert_eq!(args.output.as_deref(), Some(bili_add_on::interaction::STDOUT));
}

#[test]
fn test_cli_capture() {
    let cli = Cli::try_parse_from([
        "bili_add_on",
        "capture",
        "--capture",
        "gdigrab:desktop",
        "--range",
        "30",
        "--output",
        "out.mp4",
        "--bvid",
        "BV1test",
    ])
    .unwrap();

    let Commands::Capture(args) = cli.command else {
        panic!("expected capture");
    };
    assert_eq!(args.capture, "gdigrab:desktop");
    assert_eq!(args.range, "30");
    assert_eq!(args.output, "out.mp4");
}

#[test]
fn test_cli_list_devices() {
    let cli = Cli::try_parse_from(["bili_add_on", "list-devices", "dshow"]).unwrap();
    let Commands::ListDevices(args) = cli.command else {
        panic!("expected list-devices");
    };
    assert_eq!(args.format, "dshow");
}

#[test]
fn test_cli_parse_fails_missing_source() {
    assert!(Cli::try_parse_from(["bili_add_on", "overlay", "--input", "video.mp4"]).is_err());
}

#[test]
fn test_cli_parse_fails_missing_subcommand() {
    assert!(Cli::try_parse_from(["bili_add_on"]).is_err());
    assert!(
        Cli::try_parse_from(["bili_add_on", "overlay", "--bvid", "BV1test"]).is_err()
    );
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
