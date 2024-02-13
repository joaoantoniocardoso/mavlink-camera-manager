use tracing::*;

pub fn taskdump() {
    let tree = async_backtrace::taskdump_tree(true);

    debug!("tokio backtrace taskdump: {tree:#?}");
}
