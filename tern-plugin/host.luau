--!strict
-- Host half: the `seiji.app` block. Its state lives on the session daemon, so every window
-- attached to the session shows the same panel.

local app = require("./lib/app")

tern.block.define("app", app.def)
