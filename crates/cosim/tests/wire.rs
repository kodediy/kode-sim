//! The messages, byte for byte as the prototype that ran against esp-emu
//! 1.0.0-rc2 encoded them (scratchpad/parts/kodedot_cosim.py, 2026-11-07).

use cosim::*;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn a_registration_claims_pins_and_i2c_addresses() {
    let claims = vec![
        Claim::Gpio { drive: (1 << 29) | (1 << 6) | (1 << 35), watch: 0 },
        Claim::I2c { addr: 0x30, scl: 5, sda: 4 },
        Claim::I2c { addr: 0x20, scl: 5, sda: 4 },
    ];
    assert_eq!(
        hex(&register("kodedot", &claims)),
        "222a080112076b6f6465646f741a090a0708c080808082011a0812060830100518041a081206082010051804"
    );
}

#[test]
fn a_pin_is_driven_at_an_emulated_time() {
    assert_eq!(hex(&gpio_drive(1_500_000, 35, false)), "08e0c65b320408231000");
}

#[test]
fn an_i2c_answer_carries_its_data_or_a_nack() {
    assert_eq!(hex(&i2c_reply(7, Some(&[0xA0, 0x01]))), "420a080710011a02a0012001");
    assert_eq!(hex(&i2c_reply(8, None)), "4206080810002001");
}

#[test]
fn a_bus_request_is_read_back() {
    let raw: Vec<u8> = (0..34)
        .step_by(2)
        .map(|i| u8::from_str_radix(&"08c0c4073a0b0803120710641a01022002"[i..i + 2], 16).unwrap())
        .collect();
    let m = decode(&raw).unwrap();
    assert_eq!(m.sim_time_ns, 123_456);
    assert_eq!(
        m.payload,
        Payload::I2c(I2cRequest { id: 3, bus: 0, addr: 0x64, write: vec![0x02], read_len: 2, continues: false })
    );
}

#[test]
fn a_register_ack_and_a_pin_level_are_read_back() {
    // register_ack{proto 1, chip "esp32p4", accepted}
    let ack = [0x2a, 0x0d, 0x08, 0x01, 0x12, 0x07, b'e', b's', b'p', b'3', b'2', b'p', b'4', 0x28, 0x01];
    assert_eq!(
        decode(&ack).unwrap().payload,
        Payload::Registered { accepted: true, chip: "esp32p4".into(), reason: String::new() }
    );
    // sim_time 5, gpio_level{pin 35, level 1}
    let lvl = [0x08, 0x05, 0x1a, 0x04, 0x08, 0x23, 0x10, 0x01];
    let m = decode(&lvl).unwrap();
    assert_eq!((m.sim_time_ns, m.payload), (5, Payload::Level { pin: 35, level: true }));
}

#[test]
fn frames_are_length_prefixed_and_carry_a_topic_from_the_emulator() {
    let mut out = vec![];
    write_frame(&mut out, &[1, 2, 3]).unwrap();
    assert_eq!(out, vec![3, 0, 0, 0, 1, 2, 3]);
    let incoming = [9u8, 0, 0, 0, 3, 0, b'@', b't', b'1', 0x08, 0x05, 0x10, 0x00];
    let (topic, msg) = read_frame(&mut &incoming[..]).unwrap().unwrap();
    assert_eq!((topic.as_str(), msg), ("@t1", vec![0x08, 0x05, 0x10, 0x00]));
}

// The I2S messages, byte for byte as esp-emulator's tools/cosim_tcp.py
// (1.0.0) encodes them.

#[test]
fn an_i2s_claim_names_the_pads_the_chip_sends_and_receives_on() {
    assert_eq!(
        hex(&register("kodedot", &[Claim::I2s { dout: Some(38), din: Some(33) }])),
        "2214080112076b6f6465646f741a078a010418262021"
    );
}

#[test]
fn samples_for_the_receiver_go_out_bare() {
    assert_eq!(hex(&i2s_in(&[1, 2, 3, 4])), "fa01060a0401020304");
}

#[test]
fn an_i2s_format_and_the_samples_played_are_read_back() {
    let raw =
        |h: &str| (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
    let m = decode(&raw("0805ea010a08011080f70218022010")).unwrap();
    assert_eq!(
        (m.sim_time_ns, m.payload),
        (5, Payload::I2sFormat(I2sFormat { receive: true, rate_hz: 48000, channels: 2, bits: 16 }))
    );
    let m = decode(&raw("0809f201060a0410002000")).unwrap();
    assert_eq!((m.sim_time_ns, m.payload), (9, Payload::I2sOut(vec![0x10, 0, 0x20, 0])));
}

// The MIPI-DSI panel, byte for byte as esp-emulator's tools/cosim_tcp.py
// (1.1.0) encodes the claim and decodes the events.

#[test]
fn a_dsi_claim_names_the_virtual_channel() {
    assert_eq!(hex(&register("kodedot", &[Claim::Dsi { vc: 0 }])), "2212080112076b6f6465646f741a05a201020800");
}

#[test]
fn a_dsi_command_a_format_and_rows_are_read_back() {
    let raw =
        |h: &str| (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
    let m = decode(&raw("0805a202080a060815120251ff")).unwrap();
    assert_eq!((m.sim_time_ns, m.payload), (5, Payload::Dsi(Dsi::Command { dt: 0x15, data: vec![0x51, 0xff] })));
    let m = decode(&raw("0806a2020c120a0801109a0318f6032001")).unwrap();
    assert_eq!(
        m.payload,
        Payload::Dsi(Dsi::Format { streaming: true, width: 410, height: 502, pixels: DpiPixels::Rgb565 })
    );
    let m = decode(&raw("0807a2020e1a0c080210071801220401020304")).unwrap();
    assert_eq!(m.payload, Payload::Dsi(Dsi::Rows { frame: 2, y: 7, rows: 1, pixels: vec![1, 2, 3, 4] }));
}

// The USB host over co-simulation (esp-emu 1.3.0), byte for byte as
// esp-emulator's tools/cosim_tcp/wire.py encodes it.

#[test]
fn a_usb_host_claims_a_port() {
    assert_eq!(hex(&register("kode-emu", &[Claim::UsbHost { port: 1 }])), "2213080112086b6f64652d656d751a05c201020801");
}

#[test]
fn a_usb_transfer_carries_its_setup_its_data_or_its_length() {
    let get_device = [0x80, 6, 0, 1, 0, 0, 18, 0];
    let t =
        UsbTransfer { id: 7, endpoint: 0, setup: get_device.to_vec(), data: vec![], length: 0, timeout_us: 1_000_000 };
    assert_eq!(hex(&usb_transfer(5, &t)), "0805e20212080710001a08800600010000120030c0843d");
    let out = UsbTransfer { id: 9, endpoint: 0x02, setup: vec![], data: b"abc".to_vec(), length: 0, timeout_us: 0 };
    assert_eq!(hex(&usb_transfer(0, &out)), "0800e20209080910022203616263");
    let inn = UsbTransfer { id: 10, endpoint: 0x81, setup: vec![], data: vec![], length: 64, timeout_us: 0 };
    assert_eq!(hex(&usb_transfer(0, &inn)), "0800e20207080a1081012840");
}

#[test]
fn a_usb_link_and_a_usb_result_are_read_back() {
    let unhex =
        |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
    let m = decode(&unhex("087bda020408011001")).unwrap();
    assert_eq!((m.sim_time_ns, m.payload), (123, Payload::UsbLink { attached: true, high_speed: false }));
    let m = decode(&unhex("08c803ea020a080710011a0212012003")).unwrap();
    assert_eq!(m.payload, Payload::UsbResult { id: 7, status: UsbStatus::Ok, data: vec![0x12, 0x01], sent: 3 });
    let m = decode(&unhex("ea020408081002")).unwrap();
    assert_eq!(m.payload, Payload::UsbResult { id: 8, status: UsbStatus::Stall, data: vec![], sent: 0 });
}
