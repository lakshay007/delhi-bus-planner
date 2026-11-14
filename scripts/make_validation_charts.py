"""Draw the validation report's charts as SVG.

Usage: python3 -I scripts/make_validation_charts.py data/google/validation img

Reads by_hour.csv and pairs.csv written by `busplan validate` and writes
validation_by_hour.svg and validation_pairs.svg. Standard library only.
Colours are the first three slots of a palette checked for colour-blind
separation; text is never coloured, identity comes from the legend keys.
"""
import csv
import sys

SURFACE, INK, INK2, GRID, AXIS = "#fcfcfb", "#0b0b0b", "#52514e", "#e8e7e2", "#c9c8c1"
OURS_LIVE, GOOGLE, OURS = "#2a78d6", "#eb6834", "#1baf7a"
FONT = "-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"


def text(x, y, s, size=12, fill=INK2, anchor="start", weight="400"):
    return f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" text-anchor="{anchor}" font-weight="{weight}">{s}</text>'


def legend(x, y, items, dots=False):
    out = []
    for name, colour in items:
        if dots:
            out.append(f'<circle cx="{x + 6}" cy="{y - 4}" r="5" fill="{colour}"/>')
            x0 = x + 18
        else:
            out.append(f'<line x1="{x}" y1="{y - 4}" x2="{x + 22}" y2="{y - 4}" stroke="{colour}" stroke-width="3" stroke-linecap="round"/>')
            x0 = x + 30
        out.append(text(x0, y, name, 12.5, INK))
        x = x0 + 7.2 * len(name) + 26
    return "".join(out)


def svg(w, h, body):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="{FONT}">'
        f'<rect width="{w}" height="{h}" fill="{SURFACE}"/>{body}</svg>\n'
    )


def by_hour(src, dst):
    rows = list(csv.DictReader(open(src)))
    hours = [int(r["hour"]) for r in rows]
    W, L, R = 860, 64, 34
    pw = W - L - R
    x = lambda h: L + pw * h / 23.0
    series = [("Google Maps", GOOGLE, "google"), ("Our model, history only", OURS, "model"), ("Our model + live", OURS_LIVE, "model_live")]
    body = [text(L, 28, "Same bus legs, same real rides: how far off each estimate is, by hour of boarding", 15, INK, weight="600")]
    body.append(text(L, 47, "112,251 rides on 717 bus legs, 25–31 Oct 2026. The timetable is not drawn: it follows Google's line closely.", 12))
    body.append(legend(L, 74, [(n, c) for n, c, _ in series]))

    def panel(top, height, lo, hi, ticks, title, key, zero=False):
        y = lambda v: top + height * (hi - v) / (hi - lo)
        out = [text(L, top - 12, title, 12.5, INK, weight="600")]
        for t in ticks:
            out.append(f'<line x1="{L}" y1="{y(t):.1f}" x2="{W - R}" y2="{y(t):.1f}" stroke="{AXIS if (zero and t == 0) else GRID}" stroke-width="1"/>')
            out.append(text(L - 8, y(t) + 4, f"{t:+d}" if zero and t else str(t), 11.5, INK2, "end"))
        for name, colour, col in series:
            pts = " ".join(f"{x(h):.1f},{y(float(r[f'{col}_{key}'])):.1f}" for h, r in zip(hours, rows))
            out.append(f'<polyline points="{pts}" fill="none" stroke="{colour}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>')
        return out, y

    a, ya = panel(112, 200, 0, 15, [0, 5, 10, 15], "Average miss, minutes", "mae")
    body += a
    b, yb = panel(372, 220, -12, 16, [-10, -5, 0, 5, 10, 15], "Direction of the miss, minutes (above zero: estimate too long; below: too short)", "bias", zero=True)
    body += b

    def mark(h, col, key, yf, label, dy):
        r = rows[hours.index(h)]
        v = float(r[f"{col}_{key}"])
        colour = {"google": GOOGLE, "model": OURS, "model_live": OURS_LIVE}[col]
        return f'<circle cx="{x(h):.1f}" cy="{yf(v):.1f}" r="4.5" fill="{colour}" stroke="{SURFACE}" stroke-width="2"/>' + text(x(h), yf(v) + dy, label.format(v), 12, INK, "middle", "600")

    body.append(mark(18, "google", "mae", ya, "Google {:.1f}", -11))
    body.append(mark(18, "model_live", "mae", ya, "ours + live {:.1f}", 20))
    body.append(mark(2, "google", "mae", ya, "Google {:.1f}", -11))
    body.append(mark(2, "google", "bias", yb, "{:+.1f} at 2 am", -11))
    body.append(mark(18, "google", "bias", yb, "{:+.1f} at 6 pm", 21))
    for h in range(0, 24, 3):
        body.append(text(x(h), 372 + 220 + 20, f"{h:02d}:00", 11.5, INK2, "middle"))
        body.append(text(x(h), 112 + 200 + 18, f"{h:02d}:00", 11.5, INK2, "middle"))
    open(dst, "w").write(svg(W, 630, "".join(body)))


def pairs(src, dst):
    rows = [r for r in csv.DictReader(open(src)) if r["observed_n"] not in ("", "0")]
    W, H, L, T, S = 860, 640, 70, 96, 500
    top = 160.0
    pos = lambda v: S * v / top
    body = [text(L, 28, "30 origin–destination pairs at four times of day: estimated against actual ride time", 15, INK, weight="600")]
    body.append(text(L, 47, f"{len(rows)} pair-and-time cells, Monday–Friday 26–30 Oct 2026. Actual = median real bus ride on the same legs.", 12))
    body.append(legend(L, 74, [("Google Maps", GOOGLE), ("Our model, history only", OURS)], dots=True))
    for t in range(0, 161, 20):
        body.append(f'<line x1="{L}" y1="{T + S - pos(t):.1f}" x2="{L + S}" y2="{T + S - pos(t):.1f}" stroke="{GRID}"/>')
        body.append(f'<line x1="{L + pos(t):.1f}" y1="{T}" x2="{L + pos(t):.1f}" y2="{T + S}" stroke="{GRID}"/>')
        body.append(text(L - 8, T + S - pos(t) + 4, str(t), 11.5, INK2, "end"))
        body.append(text(L + pos(t), T + S + 18, str(t), 11.5, INK2, "middle"))
    body.append(f'<line x1="{L}" y1="{T + S}" x2="{L + S}" y2="{T}" stroke="{AXIS}" stroke-width="1.5"/>')
    body.append(text(L + S - 8, T + 36, "exactly right", 11.5, INK2, "end"))
    body.append(text(L + S / 2, T + S + 40, "Actual ride, minutes", 12.5, INK, "middle"))
    body.append(f'<text x="22" y="{T + S / 2}" font-size="12.5" fill="{INK}" text-anchor="middle" transform="rotate(-90 22 {T + S / 2})">Estimated ride, minutes</text>')
    for col, colour in (("google_ride_s", GOOGLE), ("ours_s", OURS)):
        for r in rows:
            a, p = float(r["observed_median_s"]) / 60, float(r[col]) / 60
            body.append(f'<circle cx="{L + pos(min(a, top)):.1f}" cy="{T + S - pos(min(p, top)):.1f}" r="4.5" fill="{colour}" fill-opacity="0.9" stroke="{SURFACE}" stroke-width="1.5"/>')
    # Side note: what the picture shows, in words and numbers.
    def mae(col):
        return sum(abs(float(r[col]) - float(r["observed_median_s"])) for r in rows) / len(rows) / 60
    nx = L + S + 34
    body.append(text(nx, T + 24, "Average miss", 12.5, INK, weight="600"))
    for i, (name, col, colour) in enumerate((("Google Maps", "google_ride_s", GOOGLE), ("Our model", "ours_s", OURS))):
        y = T + 52 + i * 46
        body.append(f'<circle cx="{nx + 6}" cy="{y - 5}" r="5" fill="{colour}"/>')
        body.append(text(nx + 18, y, name, 12.5, INK))
        body.append(text(nx + 18, y + 20, f"{mae(col):.1f} min", 18, INK, weight="600"))
    notes = ["Google gives one duration", "per journey whatever the", "hour, so its dots line up", "in rows while the actual", "ride ranges across the day."]
    for i, s in enumerate(notes):
        body.append(text(nx, T + 172 + i * 18, s, 12))
    open(dst, "w").write(svg(W, H, "".join(body)))


if __name__ == "__main__":
    src, out = sys.argv[1], sys.argv[2]
    by_hour(f"{src}/by_hour.csv", f"{out}/validation_by_hour.svg")
    pairs(f"{src}/pairs.csv", f"{out}/validation_pairs.svg")
    print("wrote", f"{out}/validation_by_hour.svg", f"{out}/validation_pairs.svg")
