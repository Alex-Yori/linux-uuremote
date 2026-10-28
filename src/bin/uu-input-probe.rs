// Probe: X11Input::apply with warp-based motion — pointer must land exactly.
use linux_uuremote::control::decode_control;
use linux_uuremote::input::X11Input;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;

fn pos() -> (i16, i16) {
    let (conn, idx) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[idx].root;
    conn.query_pointer(root).unwrap().reply().map(|r| (r.root_x, r.root_y)).unwrap_or((-1, -1))
}

fn main() {
    let (conn, idx) = x11rb::connect(None).unwrap();
    let mut engine = X11Input::connect(conn, idx).unwrap();
    println!("start {:?}", pos());

    for json in [
        r#"{"action":"mouse_move_absolute","abs_x":0.25,"abs_y":0.25}"#,
        r#"{"action":"mouse_move_absolute","abs_x":0.5,"abs_y":0.5}"#,
        r#"{"action":"mouse_move_absolute","abs_x":500.0,"abs_y":500.0}"#,
        r#"{"action":"mouse_move_relative","delta_x":60,"delta_y":-40}"#,
    ] {
        let msg = decode_control(json.as_bytes()).unwrap();
        let injected = engine.apply(&msg).unwrap_or(false);
        std::thread::sleep(std::time::Duration::from_millis(120));
        println!("{json} -> injected={injected} pointer={:?}", pos());
    }
    let before = pos();
    let msg = decode_control(br#"{"action":"mouse_click","button":1}"#).unwrap();
    let injected = engine.apply(&msg).unwrap_or(false);
    let after = pos();
    println!("click injected={injected} before={before:?} after={after:?} same={}", before == after);
}
