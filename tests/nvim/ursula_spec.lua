-- Headless spec for lua/ursula against a running daemon:
--   nvim --headless -u NONE --cmd "set rtp+=." -l tests/nvim/ursula_spec.lua <socket> <wav> [postprocessor]
-- <wav> is the Parakeet test_wavs/0.wav ("... observed Phebe ... the old portrait.").
-- With [postprocessor] (a configured name whose output is {"text":"POST"}) a third
-- case checks non-stream mode: only done.text is inserted.
local socket, wav, post = arg[1], arg[2], arg[3]
assert(socket and wav, "usage: ursula_spec.lua <socket> <wav> [postprocessor]")

local ursula = require("ursula")
local failed = false
local notes = {}
vim.notify = function(msg, level)
  table.insert(notes, { msg = msg, level = level })
end

local function check(name, ok, detail)
  io.stdout:write((ok and "PASS " or "FAIL ") .. name .. (detail and (" -- " .. detail) or "") .. "\n")
  failed = failed or not ok
end

local function text_of(buf)
  return table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
end

local function count(s, pat)
  return select(2, s:lower():gsub(pat, ""))
end

-- Starts a session in a fresh buffer holding `lines`, switches to another
-- buffer (insertion must still target the anchor), waits for the session end.
local function run(lines)
  vim.cmd("enew")
  local buf = vim.api.nvim_get_current_buf()
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.api.nvim_win_set_cursor(0, { 1, 0 })
  notes = {}
  vim.cmd("Ursula toggle")
  vim.cmd("enew")
  local other = vim.api.nvim_get_current_buf()
  local finished = vim.wait(60000, function()
    return not ursula.active()
  end, 20)
  return buf, finished, text_of(other)
end

-- 1. Stream mode (no post-processors): segments are inserted as they arrive.
ursula.setup({ socket = socket, source = { file = wav } })
local buf, finished, other = run({ "" })
local text = text_of(buf)
io.stdout:write("stream buffer: " .. vim.inspect(text) .. "\n")
check("stream: session finished", finished)
-- Parakeet spells the reference's "Phebe" as "Phoebe"; accept both.
check("stream: contains phebe/phoebe", count(text, "pho?ebe") >= 1)
check("stream: 'portrait' inserted exactly once", count(text, "portrait") == 1, count(text, "portrait") .. "x")
check("stream: other buffer untouched", other == "")
check("stream: no notifications", #notes == 0, vim.inspect(notes))

-- 2. Non-stream mode: only done.text (from the post-processor) is inserted.
if post then
  ursula.setup({ socket = socket, source = { file = wav }, postprocess = { post } })
  buf, finished = run({ "<>" })
  text = text_of(buf)
  io.stdout:write("postprocess buffer: " .. vim.inspect(text) .. "\n")
  check("postprocess: session finished", finished)
  check("postprocess: done.text inserted once at cursor", text == "POST<>", text)
  check("postprocess: no notifications", #notes == 0, vim.inspect(notes))
end

-- 3. Connect failure: an ERROR notification, buffer unchanged.
ursula.setup({ socket = "/nonexistent/ursula.sock", source = { file = wav } })
buf, finished = run({ "keep me" })
local msg = notes[1] and notes[1].msg or ""
io.stdout:write("connect error notify: " .. vim.inspect(notes) .. "\n")
check("connect error: session finished", finished)
check(
  "connect error: ERROR notify mentions connect",
  #notes == 1 and notes[1].level == vim.log.levels.ERROR and msg:find("connect") ~= nil,
  msg
)
check("connect error: buffer unchanged", text_of(buf) == "keep me")

io.stdout:write(failed and "FAIL ursula_spec\n" or "PASS ursula_spec\n")
os.exit(failed and 1 or 0)
