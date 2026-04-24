use frankenterm_core::policy::{is_command_candidate};
fn main() {
    println!("terraform destroy: {}", is_command_candidate("terraform destroy -auto-approve"));
    println!("helm uninstall: {}", is_command_candidate("helm uninstall my-release"));
}
