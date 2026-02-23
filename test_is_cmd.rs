use frankenterm_core::policy::is_command_candidate;
fn main() {
    println!("{}", is_command_candidate("mkfs.ext4 /dev/sdb1"));
}
