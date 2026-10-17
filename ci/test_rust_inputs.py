#!/usr/bin/env python3
"""Every workspace crate the TV library links is an input of the Makefile rule that builds it.

`$(RUST_LIB)` is an ordinary timestamp rule: make invokes cargo only when a file in
`RUST_INPUTS` is newer than the archive. A workspace member missing from that list is a crate
whose edits never rebuild the TV binary — `make` says "Nothing to be done" and `make deploy`
ships the old one. That happened to `rust-modules/dispatcharr` (2026-10-17: the account-mode fix
reached the television only because a lane touched `rust-modules/src/lib.rs` by hand).

`storage` is the one member built by a rule of its own (`pkg/dispatcharrtv-storage`, from
`STORAGE_INPUTS`), and the application crate itself is `rust-modules/src`.
"""
import os
import re
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OWN_RULE = {"storage"}


def members():
    with open(os.path.join(ROOT, "rust-modules", "Cargo.toml"), encoding="utf-8") as f:
        text = f.read()
    block = re.search(r"^members\s*=\s*\[(.*?)\]", text, re.S | re.M)
    return re.findall(r'"([^"]+)"', block.group(1))


def rust_inputs():
    with open(os.path.join(ROOT, "Makefile"), encoding="utf-8") as f:
        text = f.read()
    line = re.search(r"^RUST_INPUTS\s*:=\s*\$\(shell find (.*?) -type f", text, re.M)
    return set(line.group(1).split())


class RustInputs(unittest.TestCase):
    def test_every_linked_member_is_an_input_of_the_tv_library(self):
        inputs = rust_inputs()
        missing = [m for m in members() if m not in OWN_RULE and f"rust-modules/{m}" not in inputs]
        self.assertEqual(missing, [], "workspace members whose edits would not rebuild the TV binary")

    def test_the_application_crate_is_an_input(self):
        self.assertIn("rust-modules/src", rust_inputs())

    def test_the_parsers_see_the_files(self):
        self.assertIn("media", members())
        self.assertIn("rust-modules/media", rust_inputs())


if __name__ == "__main__":
    unittest.main()
