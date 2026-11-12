use super::*;

#[test]
fn the_clap_tree_passes_its_own_consistency_checks() {
    <Cli as clap::CommandFactory>::command().debug_assert();
}

/// Every visible help page in one snapshot.
#[test]
fn every_visible_help_page_is_snapshotted() {
    fn walk(cmd: &mut clap::Command, path: &str, out: &mut String) {
        out.push_str("$ ");
        out.push_str(path);
        out.push_str(" --help\n");
        out.push_str(&cmd.render_long_help().to_string());
        out.push('\n');
        let children: Vec<String> = cmd
            .get_subcommands()
            // clap's own `help` subcommand mirrors the whole tree; its
            // pages are the ones already snapshotted, one indirection
            // out.
            .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
            .map(|sub| sub.get_name().to_owned())
            .collect();
        for name in children {
            let child = cmd
                .find_subcommand_mut(&name)
                .unwrap_or_else(|| panic!("{name} is a subcommand"));
            walk(child, &format!("{path} {name}"), out);
        }
    }

    let mut root = <Cli as clap::CommandFactory>::command();
    root.build();
    let mut pages = String::new();
    walk(&mut root, "felis", &mut pages);
    insta::assert_snapshot!(pages);
}
