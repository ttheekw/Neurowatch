//! Neurowatch backend service entry point (see [`nw_backend::runtime`]).

fn main() -> anyhow::Result<()> {
    nw_backend::runtime::main()
}
