fn main() {
    for screen in screenshots::Screen::all().expect("screens") {
        let d = screen.display_info;
        println!(
            "id={} primary={} {}x{} at {},{} scale {}",
            d.id, d.is_primary, d.width, d.height, d.x, d.y, d.scale_factor
        );
    }
}
