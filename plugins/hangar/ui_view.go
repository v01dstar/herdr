package main

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"
)

var (
	sBold   = lipgloss.NewStyle().Bold(true)
	sDim    = lipgloss.NewStyle().Faint(true)
	sAccent = lipgloss.NewStyle().Foreground(lipgloss.Color("6"))
	sOK     = lipgloss.NewStyle().Foreground(lipgloss.Color("2"))
	sWarn   = lipgloss.NewStyle().Foreground(lipgloss.Color("3"))
	sErr    = lipgloss.NewStyle().Foreground(lipgloss.Color("1"))
	sSel    = lipgloss.NewStyle().Reverse(true)
	sButton = lipgloss.NewStyle().Reverse(true).Bold(true)
)

func colour(kind string) lipgloss.Style {
	switch kind {
	case "ok":
		return sOK
	case "warn":
		return sWarn
	case "err":
		return sErr
	}
	return sDim
}

// Mouse targets recorded by the last render. View has a value receiver, so
// they live behind a pointer.
type box struct{ x0, x1, y int }

func (b box) has(x, y int) bool { return y == b.y && x >= b.x0 && x < b.x1 }

type button struct {
	box
	id string
}

type optionHit struct {
	box
	c *choice
	i int
}

type hits struct {
	tabs    []box
	rows    map[int]int
	actions map[int]int
	listX1  int
	buttons []button
	fields  map[int]int
	options []optionHit
}

func (h *hits) reset() {
	*h = hits{rows: map[int]int{}, actions: map[int]int{}, fields: map[int]int{}}
}

// canvas is a fixed grid of styled lines.
type canvas struct {
	w     int
	lines []string
}

func newCanvas(w, h int) *canvas {
	c := &canvas{w: w, lines: make([]string, h)}
	return c
}

func (c *canvas) set(y int, s string) {
	if y >= 0 && y < len(c.lines) {
		c.lines[y] = s
	}
}

func (c *canvas) String() string {
	out := make([]string, len(c.lines))
	for i, l := range c.lines {
		out[i] = fit(l, c.w)
	}
	return strings.Join(out, "\n")
}

func (m model) View() string {
	if m.w == 0 || m.h == 0 {
		return ""
	}
	if m.w < 60 || m.h < 18 {
		return "Make this pane larger (at least 60×18) for hangar settings."
	}
	h := m.hit
	h.reset()
	c := newCanvas(m.w, m.h)

	badge := ""
	switch {
	case m.acct.SignedIn():
		badge = "@" + m.acct.Login
	case m.checked && m.acct.SignedOut:
		badge = sDim.Render("not signed in")
	}
	var tabs strings.Builder
	tabs.WriteString(" ")
	x := 1
	for i, name := range tabNames {
		label := " " + name + " "
		if i == m.tab {
			tabs.WriteString(sSel.Render(label))
		} else {
			tabs.WriteString(label)
		}
		h.tabs = append(h.tabs, box{x, x + len(label), 0})
		tabs.WriteString(" ")
		x += len(label) + 1
	}
	// herdr's pane frame already carries the title; the sign-in badge sits at
	// the end of the tab strip.
	c.set(0, spread(tabs.String(), badge+" ", m.w))
	c.set(1, sDim.Render(strings.Repeat("─", m.w)))

	top, bottom := 2, m.h-5
	switch m.tab {
	case tabRemotes:
		m.renderRemotes(c, top, bottom)
	case tabImages:
		m.renderImages(c, top, bottom)
	default:
		m.renderAccount(c, top, bottom)
	}

	msg := m.footerMessage()
	for i, line := range wrap(msg.text, m.w-2) {
		if i < 2 {
			c.set(m.h-4+i, " "+msg.style.Render(line))
		}
	}
	c.set(m.h-2, sDim.Render(strings.Repeat("─", m.w)))
	closeBtn := " esc close "
	h.buttons = append(h.buttons, button{box{1, 1 + len(closeBtn), m.h - 1}, "close"})
	c.set(m.h-1, spread(" "+sButton.Render(closeBtn), sDim.Render(m.hint())+" ", m.w))

	if m.dialog != nil {
		m.renderDialog(c)
	}
	return c.String()
}

type styled struct {
	text  string
	style lipgloss.Style
}

// footerMessage is the message strip: running jobs, the last result, or the
// tab's note.
func (m model) footerMessage() styled {
	for _, j := range m.jobs {
		if j.Status == "running" && j.Progress != "" && m.message == "Working… Esc closes this pane; the operation continues." {
			return styled{j.Progress + " Esc closes this pane; the operation continues.", sWarn}
		}
	}
	if m.message != "" {
		switch m.kind {
		case msgErr:
			return styled{m.message, sErr}
		case msgOK:
			return styled{m.message, sOK}
		}
		return styled{m.message, sWarn}
	}
	if m.tab == tabAccount {
		return styled{accountNote, sDim}
	}
	return styled{"", sDim}
}

func (m model) hint() string {
	switch {
	case m.dialog != nil:
		return "tab/↑↓ field  ↵ confirm  esc back"
	case m.tab == tabAccount:
		return "↑↓ select  ↵ run  ←→ tab"
	case m.focus == 1:
		return "↑↓ select  ↵ run  esc back to list  ←→ tab"
	}
	return "↑↓ select  ↵ actions  ←→ tab"
}

func (m model) listWidth() int { return max(18, min(30, m.w/3)) }

func (m model) renderRemotes(c *canvas, top, bottom int) {
	lw := m.listWidth()
	m.hit.listX1 = lw
	rows := m.rows()
	height := bottom - top + 1
	first := 0
	if m.sel >= height {
		first = m.sel - height + 1
	}
	left := make([]string, height)
	for i := first; i < len(rows) && i-first < height; i++ {
		r := rows[i]
		marker := "  "
		if i == m.sel {
			marker = "▸ "
		}
		var name, right string
		switch r.kind {
		case rowLocal:
			name = "Local"
		case rowAdd:
			name = sAccent.Render("+ Add remote")
		case rowSSH:
			name = "⇄ " + r.profile.Label
			if r.profile.Enabled {
				right = sOK.Render("enabled")
			} else {
				right = sDim.Render("disabled")
			}
		case rowHangar:
			name = "⇄ " + m.st.label(r.machine)
			word, kind := m.stateWord(r)
			right = colour(kind).Render(word)
		}
		var notes []string
		if r.kind == rowHangar && m.st.Hidden[r.machine.ID] {
			notes = append(notes, "hidden")
		}
		if r.kind != rowAdd && m.isDefault(r) && (r.kind != rowLocal || m.st.Default == "" || !m.defaultValid()) {
			notes = append(notes, "default")
		}
		line := spread(marker+name, right+" ", lw)
		if len(notes) > 0 {
			// Notes go first when the row is too narrow for all of it.
			withNotes := sDim.Render(strings.Join(notes, " · ")) + " " + right
			if lipgloss.Width(marker+name)+lipgloss.Width(withNotes)+2 <= lw {
				line = spread(marker+name, withNotes+" ", lw)
			}
		}
		if i == m.sel {
			if m.focus == 0 {
				line = sSel.Render(fit(ansi.Strip(line), lw))
			} else {
				line = sBold.Render(fit(ansi.Strip(line), lw))
			}
		}
		left[i-first] = line
		m.hit.rows[top+i-first] = i
	}
	right := make([]string, height)
	if r, ok := m.selectedRow(); ok {
		header, info := m.detailLines(r)
		m.renderDetail(right, top, m.w-lw-1, header, info, m.remoteActions(), m.act, m.focus == 1)
	}
	for i := 0; i < height; i++ {
		c.set(top+i, fit(left[i], lw)+sDim.Render("│")+right[i])
	}
}

// renderDetail draws a header, info lines and an action column into out, and
// records where the actions are.
func (m model) renderDetail(out []string, top, w int, header string, info []string, acts []action, sel int, focused bool) {
	y := 0
	put := func(s string) {
		if y < len(out) {
			out[y] = s
		}
		y++
	}
	put(" " + sBold.Render(truncate(header, w-2)))
	for _, line := range info {
		for _, l := range wrap(line, w-2) {
			put(" " + sDim.Render(l))
		}
	}
	put("")
	group := -1
	for i, a := range acts {
		if group >= 0 && a.group != group {
			put("")
		}
		group = a.group
		label := "  " + a.label + "  "
		switch {
		case i == sel && focused:
			label = sSel.Render(label)
		case a.reason != "":
			label = sDim.Render(label)
		}
		if y < len(out) {
			m.hit.actions[top+y] = i
		}
		put(" " + label)
	}
	if focused && sel < len(acts) && acts[sel].reason != "" {
		put("")
		for _, l := range wrap("↳ "+acts[sel].reason, w-3) {
			put("  " + sWarn.Render(l))
		}
	}
}

func (m model) renderImages(c *canvas, top, bottom int) {
	lw := m.listWidth()
	m.hit.listX1 = lw
	height := bottom - top + 1
	left := make([]string, height)
	right := make([]string, height)
	if listNote, detail := m.imagesPlaceholder(); detail != "" {
		left[0] = " " + sDim.Render(listNote)
		for i, l := range wrap(detail, m.w-lw-3) {
			if i < height {
				right[i] = " " + sDim.Render(l)
			}
		}
	} else {
		first := 0
		if m.img >= height {
			first = m.img - height + 1
		}
		for i := first; i < len(m.images) && i-first < height; i++ {
			im := m.images[i]
			marker := "  "
			if i == m.img {
				marker = "▸ "
			}
			line := spread(marker+im.Name, sDim.Render(im.CreatedAt.Local().Format("2006-01-02"))+" ", lw)
			if i == m.img {
				if m.focus == 0 {
					line = sSel.Render(fit(ansi.Strip(line), lw))
				} else {
					line = sBold.Render(fit(ansi.Strip(line), lw))
				}
			}
			left[i-first] = line
			m.hit.rows[top+i-first] = i
		}
		header, info := m.imageDetail(m.images[m.img])
		m.renderDetail(right, top, m.w-lw-1, header, info, m.imageActions(), m.imgAc, m.focus == 1)
	}
	for i := 0; i < height; i++ {
		c.set(top+i, fit(left[i], lw)+sDim.Render("│")+right[i])
	}
}

func (m model) renderAccount(c *canvas, top, bottom int) {
	y := top + 1
	for _, l := range wrap(m.accountSummary(), m.w-4) {
		c.set(y, "  "+sBold.Render(l))
		y++
	}
	y++
	for i, a := range m.accountActions() {
		label := "  " + a.label + "  "
		if i == m.accAc {
			label = sSel.Render(label)
		}
		c.set(y, "  "+label)
		m.hit.actions[y] = i
		y++
	}
	y++
	if y <= bottom {
		c.set(y, "  "+sBold.Render("Usage"))
		y++
	}
	for _, l := range m.usageLines() {
		if y <= bottom {
			c.set(y, "  "+sDim.Render(l))
			y++
		}
	}
	ssh := "SSH config: ~/.ssh/config includes the hangar hosts."
	if !sshIncluded() {
		ssh = "SSH config: signing in adds one Include line to ~/.ssh/config; signing out removes it."
	}
	if y+1 <= bottom {
		c.set(y+1, "  "+sDim.Render(truncate(ssh, m.w-4)))
	}
}

// ---- dialogs ----

func (m model) renderDialog(c *canvas) {
	d := m.dialog
	dw := min(m.w-4, 76)
	inner := dw - 4
	var body []string
	var title string
	var buttons []button

	switch {
	case d.confirm != nil:
		title = d.confirm.title
		body = append(body, wrap(d.confirm.body, inner)...)
		body = append(body, "")
		buttons = []button{{id: " ↵ " + d.confirm.verb + " "}, {id: " esc cancel "}}
	case d.chooser != nil:
		title, body, buttons = m.chooserBody(d.chooser, inner)
	default:
		title, body, buttons = m.formBody(d.form, inner)
	}

	height := len(body) + 4
	x0 := (m.w - dw) / 2
	y0 := max(1, (m.h-height)/2)
	border := func(left, fill, right string) string {
		return strings.Repeat(" ", x0) + left + fill + right
	}
	t := " " + title + " "
	c.set(y0, border("┌", "─"+sBold.Render(t)+strings.Repeat("─", max(0, dw-3-len([]rune(t)))), "┐"))
	for i, l := range body {
		y := y0 + 1 + i
		c.set(y, border("│", " "+fit(l, dw-3), "│"))
		// Field and option targets were recorded relative to the body.
		m.shiftHits(i, y, x0+2)
	}
	// Buttons, right-aligned.
	by := y0 + 1 + len(body)
	labels := make([]string, len(buttons))
	width := 0
	for i, b := range buttons {
		labels[i] = b.id
		width += len([]rune(b.id))
		if i > 0 {
			width += 2
		}
	}
	bx := x0 + dw - 2 - width
	var row strings.Builder
	row.WriteString(strings.Repeat(" ", bx-x0-1))
	for i, label := range labels {
		if i > 0 {
			row.WriteString("  ")
			bx += 2
		}
		style := sBold
		switch {
		case strings.Contains(label, "↵") && m.primaryDisabled():
			style = sDim
		case strings.Contains(label, "↵"):
			style = sButton
		}
		row.WriteString(style.Render(label))
		n := len([]rune(label))
		id := "cancel"
		if strings.Contains(label, "↵") {
			id = "primary"
		}
		m.hit.buttons = append(m.hit.buttons, button{box{bx, bx + n, by}, id})
		bx += n
	}
	c.set(by, border("│", fit(row.String(), dw-2), "│"))
	c.set(by+1, border("│", strings.Repeat(" ", dw-2), "│"))
	c.set(by+2, border("└", strings.Repeat("─", dw-2), "┘"))
}

// Hits recorded while building a dialog body use the body line index as y and
// a column relative to the body; shiftHits moves them to the screen.
func (m model) shiftHits(line, y, x int) {
	for i := range m.hit.options {
		o := &m.hit.options[i]
		if o.y == -1-line {
			o.y, o.x0, o.x1 = y, o.x0+x, o.x1+x
		}
	}
	for i := range m.hit.buttons {
		b := &m.hit.buttons[i]
		if b.y == -1-line {
			b.y, b.x0, b.x1 = y, b.x0+x, b.x1+x
		}
	}
	if f, ok := m.hit.fields[-1-line]; ok {
		delete(m.hit.fields, -1-line)
		m.hit.fields[y] = f
	}
}

func (m model) primaryDisabled() bool {
	f := m.dialog.form
	return f != nil && (f.kind == formClone || f.kind == formImage) && (f.plan == "" || f.plan == "none")
}

func (m model) chooserBody(ch *chooser, inner int) (string, []string, []button) {
	colW := (inner - 3) / 2
	opts := [2][3]string{
		{"Clone now", "a second machine", "exactly like this one"},
		{"Save as image", "a starting point for", "new machines"},
	}
	var body []string
	body = append(body, "")
	lines := [3]string{}
	for i, o := range opts {
		t := fit(" "+o[0], colW)
		if i == ch.pick {
			t = sSel.Render(t)
		} else {
			t = sBold.Render(t)
		}
		lines[0] += t
		lines[1] += fit(" "+sDim.Render(o[1]), colW)
		lines[2] += fit(" "+sDim.Render(o[2]), colW)
		if i == 0 {
			for j := range lines {
				lines[j] += "   "
			}
		}
		for j := range lines {
			m.hit.buttons = append(m.hit.buttons, button{box{i * (colW + 3), i*(colW+3) + colW, -1 - len(body) - j}, fmt.Sprintf("pick%d", i)})
		}
	}
	body = append(body, lines[0], lines[1], lines[2], "")
	table := [][3]string{
		{"", "Clone", "Image"},
		{"Installed software", "✓", "✓"},
		{"System settings", "✓", "✓"},
		{"Repos & home files", "✓", "✗"},
		{"Logins (gh, claude)", "✓", "✗"},
		{"Result", "new machine", "reusable image"},
	}
	for i, r := range table {
		line := fit("  "+r[0], 24) + fit(r[1], 14) + r[2]
		if i == 0 {
			line = sBold.Render(line)
		} else if i == len(table)-1 {
			line = sDim.Render(line)
		}
		body = append(body, line)
	}
	body = append(body, "", sDim.Render("←→ switch"))
	return "copy " + ch.label, body, []button{{id: " ↵ continue "}, {id: " esc cancel "}}
}

func (m model) formBody(f *form, inner int) (string, []string, []button) {
	labelW := 15
	valueW := inner - labelW - 4
	var body []string
	body = append(body, "")
	for i := range f.fields {
		fl := &f.fields[i]
		marker := "  "
		if i == f.focus {
			marker = sAccent.Render("▸ ")
		}
		var value string
		if c := fl.choice; c != nil {
			v := "[ " + truncate(c.options[c.sel], valueW-6) + " ▾ ]"
			if i == f.focus {
				v = sSel.Render(v)
			}
			value = v
		} else {
			fl.input.Width = valueW - 1
			value = fl.input.View()
		}
		m.hit.fields[-1-len(body)] = i
		body = append(body, marker+fit(fl.label, labelW)+value)
		if c := fl.choice; c != nil && c.open {
			for j, o := range c.options {
				line := "    " + strings.Repeat(" ", labelW) + truncate(o, valueW-2)
				if j == c.hover {
					line = "    " + strings.Repeat(" ", labelW) + sSel.Render(fit(truncate(o, valueW-2), valueW-2))
				}
				m.hit.options = append(m.hit.options, optionHit{box{0, inner, -1 - len(body)}, c, j})
				body = append(body, line)
			}
		}
		body = append(body, "")
	}
	text := f.text
	if f.kind == formAdd {
		text = m.addHint(f)
	}
	for _, l := range wrap(text, inner) {
		body = append(body, sDim.Render(l))
	}
	if f.check != "" {
		body = append(body, "")
		for _, l := range wrap(f.check, inner) {
			body = append(body, sWarn.Render(l))
		}
	}
	if f.err != "" {
		body = append(body, "")
		for _, l := range wrap(f.err, inner) {
			body = append(body, sErr.Render(l))
		}
	}
	body = append(body, "")
	primary := f.primary
	if f.kind == formAdd && !m.acct.SignedIn() {
		primary = "sign in"
	}
	cancel := " esc cancel "
	if f.kind == formClone || f.kind == formImage {
		cancel = " esc back "
	}
	return f.title, body, []button{{id: " ↵ " + primary + " "}, {id: cancel}}
}

// ---- mouse ----

func (m model) mouse(ev tea.MouseMsg) (tea.Model, tea.Cmd) {
	if ev.Action != tea.MouseActionPress {
		return m, nil
	}
	if m.dialog == nil {
		switch ev.Button {
		case tea.MouseButtonWheelUp:
			m.move(-1)
			return m, nil
		case tea.MouseButtonWheelDown:
			m.move(1)
			return m, nil
		}
	}
	if ev.Button != tea.MouseButtonLeft {
		return m, nil
	}
	x, y := ev.X, ev.Y
	h := m.hit
	if d := m.dialog; d != nil {
		for _, o := range h.options {
			if o.has(x, y) {
				return m.pickChoice(o.c, o.i)
			}
		}
		for _, b := range h.buttons {
			if !b.has(x, y) {
				continue
			}
			switch {
			case b.id == "primary" && d.confirm != nil:
				return m.acceptConfirm()
			case b.id == "primary" && d.chooser != nil:
				return m.openCopyForm(d.chooser)
			case b.id == "primary":
				return m.submit()
			case b.id == "cancel":
				return m.dialogKey(tea.KeyMsg{Type: tea.KeyEsc})
			case strings.HasPrefix(b.id, "pick") && d.chooser != nil:
				pick := int(b.id[4] - '0')
				if d.chooser.pick == pick {
					return m.openCopyForm(d.chooser)
				}
				d.chooser.pick = pick
				return m, nil
			}
		}
		if f := d.form; f != nil {
			if i, ok := h.fields[y]; ok {
				f.focus = i
				f.syncFocus()
				if c := f.fields[i].choice; c != nil {
					c.open, c.hover = !c.open, c.sel
				}
			}
		}
		return m, nil
	}
	for _, b := range h.buttons {
		if b.has(x, y) && b.id == "close" {
			return m, tea.Quit
		}
	}
	for i, t := range h.tabs {
		if t.has(x, y) {
			return m.switchTab(i - m.tab)
		}
	}
	if i, ok := h.actions[y]; ok && (m.tab == tabAccount || x > h.listX1) {
		switch m.tab {
		case tabRemotes:
			m.focus, m.act = 1, i
			return m.runRemoteAction()
		case tabImages:
			m.focus, m.imgAc = 1, i
			return m.runImageAction()
		default:
			m.accAc = i
			return m.runAccountAction()
		}
	}
	if i, ok := h.rows[y]; ok && x < h.listX1 {
		if m.tab == tabRemotes {
			m.sel, m.act, m.focus, m.selID = i, 0, 0, ""
			if r, _ := m.selectedRow(); r.kind == rowAdd {
				return m.openAdd("")
			}
		} else {
			m.img, m.imgAc, m.focus = i, 0, 0
		}
	}
	return m, nil
}

// ---- text helpers ----

// fit pads or truncates a styled string to exactly w cells.
func fit(s string, w int) string {
	if w <= 0 {
		return ""
	}
	if n := lipgloss.Width(s); n > w {
		return ansi.Truncate(s, w, "")
	} else {
		return s + strings.Repeat(" ", w-n)
	}
}

func truncate(s string, w int) string {
	if lipgloss.Width(s) <= w {
		return s
	}
	return ansi.Truncate(s, max(0, w-1), "") + "…"
}

// spread puts left and right at the two ends of a w-cell row.
func spread(left, right string, w int) string {
	gap := w - lipgloss.Width(left) - lipgloss.Width(right)
	if gap < 1 {
		return fit(left, w)
	}
	return left + strings.Repeat(" ", gap) + right
}

func wrap(s string, w int) []string {
	if s == "" || w <= 0 {
		return nil
	}
	return strings.Split(ansi.Wordwrap(s, w, ""), "\n")
}
