-- Ursula reference client: talks NDJSON to the daemon over its Unix socket
-- with vim.uv only (no external processes, no plugin deps).
--
--   require("ursula").setup({ postprocess = { "cleanup" } })
--   vim.keymap.set({ "n", "i" }, "<F9>", require("ursula").toggle)
--
-- Text is inserted at the cursor position captured on start (before the
-- character under a normal-mode cursor), in that buffer, wherever you are when
-- events arrive. Stream mode (no post-processors) inserts each `segment` as it
-- arrives; otherwise `done.text` is inserted once at the end. After each insert
-- the starting window's cursor moves to the end of the text, and starting again
-- from there appends. A red dot (hl group `UrsulaRecording`) sits in the
-- top-right corner while a session is active.
local M = {}

local uv = vim.uv or vim.loop
local api = vim.api
local ns = api.nvim_create_namespace("ursula")
local ERROR = vim.log.levels.ERROR

local defaults = {
  socket = (vim.env.XDG_RUNTIME_DIR or "") .. "/ursula.sock",
  postprocess = {},
  hotwords = {},
  context_lines = 20,
  source = "mic", -- or { file = "/abs/path.wav" }
}
M.config = vim.deepcopy(defaults)

-- The one active session of this nvim, or nil. Every callback captures its own
-- session table `s` and does nothing unless `S == s`, so events arriving after
-- done/cancelled/error (or for an older session) are ignored.
local S = nil
-- { buf, mark } marking the end of the previous session's text, or nil.
local last = nil

local function send(s, msg)
  if not s.pipe:is_closing() then
    s.pipe:write(vim.json.encode(msg) .. "\n")
  end
end

local function finish(s)
  if S ~= s then
    return
  end
  S = nil
  if not s.pipe:is_closing() then
    s.pipe:close()
  end
  last = { buf = s.buf, mark = s.mark }
  pcall(api.nvim_win_close, s.dot, true)
  pcall(api.nvim_del_autocmd, s.au)
end

local function insert(s, text)
  if text == "" or not api.nvim_buf_is_valid(s.buf) then
    return
  end
  local ok, err = pcall(function()
    local pos = api.nvim_buf_get_extmark_by_id(s.buf, ns, s.mark, {})
    local lines = vim.split(text, "\n", { plain = true })
    api.nvim_buf_set_text(s.buf, pos[1], pos[2], pos[1], pos[2], lines)
    if api.nvim_win_is_valid(s.win) and api.nvim_win_get_buf(s.win) == s.buf then
      local e = api.nvim_buf_get_extmark_by_id(s.buf, ns, s.mark, {})
      api.nvim_win_set_cursor(s.win, { e[1] + 1, e[2] })
    end
  end)
  if not ok then
    -- Don't lose the speech: show it so it can be copied from :messages.
    vim.notify("ursula: insert failed (" .. tostring(err) .. "): " .. text, ERROR)
  end
end

local function handle(s, line)
  if S ~= s then
    return
  end
  local ok, ev = pcall(vim.json.decode, line)
  if not ok or type(ev) ~= "table" then
    return
  end
  if ev.event == "segment" and s.stream then
    insert(s, ev.text .. " ")
  elseif ev.event == "done" then
    if not s.stream then
      insert(s, ev.text)
    end
    finish(s)
  elseif ev.event == "error" then
    vim.notify(tostring(ev.code) .. ": " .. tostring(ev.message), ERROR)
    finish(s)
  elseif ev.event == "cancelled" then
    finish(s)
  end
end

-- Runs in libuv's fast context: only pure Lua here, API work via vim.schedule.
local function on_read(s, err, chunk)
  if err or not chunk then
    return vim.schedule(function()
      if S == s then
        vim.notify("ursula: connection closed" .. (err and (": " .. err) or ""), ERROR)
        finish(s)
      end
    end)
  end
  s.rx = s.rx .. chunk
  for line in s.rx:gmatch("([^\n]*)\n") do
    vim.schedule(function()
      handle(s, line)
    end)
  end
  s.rx = s.rx:match("[^\n]*$") -- keep the partial frame after the last \n
end

-- Where to anchor a new session: the cursor, or the end of the previous
-- session's text when the cursor is still there. Normal mode clamps a cursor
-- past the end of the line onto the last character, so that counts too.
local function anchor(buf, row, col)
  local prev = last
  last = nil
  if not prev or prev.buf ~= buf or not api.nvim_buf_is_valid(buf) then
    return col
  end
  local ok, p = pcall(api.nvim_buf_get_extmark_by_id, buf, ns, prev.mark, {})
  pcall(api.nvim_buf_del_extmark, buf, ns, prev.mark)
  if not ok or p[1] ~= row - 1 then
    return col
  end
  local len = #api.nvim_buf_get_lines(buf, p[1], p[1] + 1, true)[1]
  if col == p[2] or (col == p[2] - 1 and p[2] == len) then
    return p[2]
  end
  return col
end

local function show_dot()
  api.nvim_set_hl(0, "UrsulaRecording", { fg = "Red", default = true })
  local b = api.nvim_create_buf(false, true)
  vim.bo[b].bufhidden = "wipe"
  api.nvim_buf_set_lines(b, 0, -1, false, { "●" })
  local w = api.nvim_open_win(b, false, {
    relative = "editor",
    anchor = "NE",
    row = 0,
    col = vim.o.columns,
    width = 1,
    height = 1,
    style = "minimal",
    focusable = false,
    noautocmd = true,
    zindex = 250,
  })
  vim.wo[w].winhighlight = "Normal:UrsulaRecording"
  return w
end

local function start()
  local cfg = M.config
  local buf = api.nvim_get_current_buf()
  local row, col = unpack(api.nvim_win_get_cursor(0))
  col = anchor(buf, row, col)
  local half = math.floor(cfg.context_lines / 2)
  local around = api.nvim_buf_get_lines(buf, math.max(0, row - 1 - half), row + half, false)
  local s = {
    pipe = assert(uv.new_pipe(false)),
    buf = buf,
    win = api.nvim_get_current_win(),
    dot = show_dot(),
    stream = #cfg.postprocess == 0,
    rx = "",
    mark = api.nvim_buf_set_extmark(buf, ns, row - 1, col, { right_gravity = true }),
  }
  s.au = api.nvim_create_autocmd("BufWipeout", {
    buffer = buf,
    once = true,
    callback = function()
      if S == s then
        send(s, { op = "cancel" })
      end
    end,
  })
  S = s
  s.pipe:connect(cfg.socket, function(err)
    if err then
      return vim.schedule(function()
        if S == s then
          vim.notify("ursula: cannot connect to " .. cfg.socket .. ": " .. err, ERROR)
          finish(s)
        end
      end)
    end
    s.pipe:read_start(function(rerr, chunk)
      on_read(s, rerr, chunk)
    end)
  end)
  -- libuv queues writes until the connect completes (and cancels them if it
  -- fails), so a stop/cancel sent right after this still follows `start`.
  send(s, {
    op = "start",
    source = cfg.source,
    stream = s.stream,
    hotwords = cfg.hotwords,
    postprocess = cfg.postprocess,
    context = "filetype: " .. vim.bo[buf].filetype .. "\n" .. table.concat(around, "\n"),
  })
end

function M.active()
  return S ~= nil
end

function M.stop()
  if S then
    send(S, { op = "stop" })
  end
end

function M.cancel()
  if S then
    send(S, { op = "cancel" })
  end
end

function M.toggle()
  if S then
    M.stop()
  else
    start()
  end
end

function M.setup(opts)
  M.config = vim.tbl_extend("force", defaults, opts or {})
  local cmds = { toggle = M.toggle, stop = M.stop, cancel = M.cancel }
  api.nvim_create_user_command("Ursula", function(a)
    local f = cmds[a.args]
    if not f then
      return vim.notify("Ursula: expected toggle|stop|cancel, got " .. a.args, ERROR)
    end
    f()
  end, {
    nargs = 1,
    complete = function()
      return { "toggle", "stop", "cancel" }
    end,
  })
end

return M
