use frankenterm_core::command_guard::evaluate_stateless;
fn main() {
    println!("{:?}", evaluate_stateless("rm -rf target; rm -rf /"));
}
