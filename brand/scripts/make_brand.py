"""Write the dotonce mark, icon and badge SVGs, and the README cover pages, into <repo>/brand.

The mark is the music repeat sign with one dot taken away: it plays once.
Usage: python3 brand/scripts/make_brand.py brand
"""
import os
import sys

out = sys.argv[1]
for sub in ('mark', 'icon', 'cover'):
    os.makedirs(os.path.join(out, sub), exist_ok=True)

GREEN, FOREST, LIME, INK, PAPER = '#136b3c', '#003820', '#bef050', '#0f1f17', '#f4f7f2'
VIEW = '12 12 76 76'


def bar(x, y, w, h, fill):
    return f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{min(w, h) / 2}" fill="{fill}"/>'


def dot(x, y, r, fill, opacity=1):
    return f'<circle cx="{x}" cy="{y}" r="{r}" fill="{fill}" opacity="{opacity}"/>'


def mark(fill):
    return bar(24, 18, 17, 64, fill) + bar(49, 18, 8, 64, fill) + dot(72, 41, 8.5, fill)


def repeat_sign(fill):
    return mark(fill) + dot(72, 60, 8.5, fill)


def write(path, svg):
    with open(os.path.join(out, path), 'w') as f:
        f.write(svg + '\n')


def svg(view, body, title, size=None):
    wh = f' width="{size}" height="{size}"' if size else ''
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{view}"{wh}><title>{title}</title>{body}</svg>'


for name, fill in (('green', GREEN), ('white', '#fff'), ('ink', INK), ('lime', LIME)):
    write(f'mark/dotonce-mark-{name}.svg', svg(VIEW, mark(fill), f'dotonce mark, {name}'))


def tile(bg, fill):
    return (f'<rect width="100" height="100" rx="23" fill="{bg}"/>'
            f'<g transform="translate(50 50) scale(.8) translate(-48 -50)">{mark(fill)}</g>')


def badge(fill, cut):
    return (f'<defs><mask id="{cut}"><rect width="100" height="100" fill="#fff"/>'
            f'<g transform="translate(50 50) scale(.64) translate(-48 -50)">{mark("#000")}</g></mask></defs>'
            f'<circle cx="50" cy="50" r="46" fill="{fill}" mask="url(#{cut})"/>')


write('icon/dotonce-icon-forest.svg', svg('0 0 100 100', tile(FOREST, LIME), 'dotonce icon, forest', 1024))
write('icon/dotonce-icon-green.svg', svg('0 0 100 100', tile(GREEN, '#fff'), 'dotonce icon, green', 1024))
write('icon/dotonce-icon-ink.svg', svg('0 0 100 100', tile(INK, '#fff'), 'dotonce icon, ink', 1024))
write('icon/dotonce-badge-green.svg', svg('0 0 100 100', badge(GREEN, 'g'), 'dotonce badge, green', 1024))
write('icon/dotonce-badge-lime.svg', svg('0 0 100 100', badge(LIME, 'l'), 'dotonce badge, lime', 1024))


def cover(theme):
    dark = theme == 'dark'
    bg, fg, sub, m = (FOREST, '#ffffff', '#b9d3c1', LIME) if dark else (PAPER, INK, '#3d5246', GREEN)
    faint = 0.28 if dark else 0.22
    story = (f'<svg viewBox="12 12 76 76" width="230" height="230">{repeat_sign(m)}</svg>'
             f'<span class="arrow" style="color:{sub}">plays again</span>'
             f'<svg viewBox="12 12 76 76" width="230" height="230">{mark(m)}{dot(72, 60, 8.5, m, faint)}</svg>'
             f'<span class="arrow" style="color:{sub}">plays once</span>')
    return f'''<!doctype html><meta charset="utf-8">
<link href="https://fonts.googleapis.com/css2?family=Nunito:wght@800&family=Inter:wght@500&display=swap" rel="stylesheet">
<style>
html,body{{margin:0;width:2560px;height:1280px;background:{bg};overflow:hidden}}
.wrap{{position:absolute;inset:0;padding:180px 200px;display:flex;flex-direction:column;justify-content:center}}
.lock{{display:flex;align-items:center;gap:28px;font:800 112px/1 Nunito,sans-serif;letter-spacing:-.02em;color:{fg}}}
h1{{font:800 124px/1.05 Nunito,sans-serif;letter-spacing:-.025em;color:{fg};margin:110px 0 36px;max-width:1760px;white-space:nowrap}}
p{{font:500 46px/1.45 Inter,sans-serif;color:{sub};margin:0;max-width:1300px}}
.story{{position:absolute;right:200px;top:50%;transform:translateY(-50%);display:flex;flex-direction:column;align-items:center;gap:18px}}
.arrow{{font:500 34px/1 Inter,sans-serif}}
</style>
<div class="wrap"><div class="lock"><svg viewBox="16 14 70 72" width="128" height="132">{mark(m)}</svg>dotonce</div>
<h1>Correct your agent once.</h1>
<p>Memory and mistake checks for AI coding agents. Local, fast, and open source.</p></div>
<div class="story">{story}</div>'''


write('cover/cover-light.html', cover('light'))
write('cover/cover-dark.html', cover('dark'))
