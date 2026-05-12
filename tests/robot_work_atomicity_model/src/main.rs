use robot_work_atomicity_model::RobotWorkAtomicityModel;
use stateright::{Checker, Model};

fn main() {
    let checker = RobotWorkAtomicityModel::smoke()
        .checker()
        .threads(1)
        .target_max_depth(14)
        .spawn_bfs()
        .join();
    checker.assert_properties();
    println!(
        "{{\"status\":\"pass\",\"state_count\":{},\"unique_state_count\":{},\"max_depth\":{}}}",
        checker.state_count(),
        checker.unique_state_count(),
        checker.max_depth()
    );
}
