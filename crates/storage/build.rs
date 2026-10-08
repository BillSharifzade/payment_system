// `sqlx::migrate!` embeds the migrations at compile time; without this a new
// migration file would not trigger a rebuild.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
