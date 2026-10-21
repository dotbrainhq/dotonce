# dotonce brand assets

The dotonce mark is called Once: the music repeat sign with one dot taken away. Two bars and two dots tell a player to play it again. With one dot gone, it plays once, the way dotonce lets a mistake happen once. The dot is also the Dot in Dotbrain.

## Files

| Path | What it is |
|---|---|
| `mark/` | The mark on its own, in green, white, ink and lime. |
| `icon/` | App icons on the forest, green and ink tiles, and the round Dotbrain-style badge in green and lime. |
| `cover/` | The pages the README covers are rendered from. |
| `png/sizes/` | The forest icon at 16, 32, 48, 128, 256, 512 and 1024 px. |
| `scripts/make_brand.py` | Writes every SVG and the cover pages. |

## Colors

- **Brand green:** `#136b3c`, the Dotbrain green.
- **Forest:** `#003820`, the dark tile and dark cover.
- **Lime:** `#bef050`, the mark on forest.
- **Ink:** `#0f1f17`, text and the one-color dark mark.

The wordmark is set in Nunito ExtraBold, released under the SIL Open Font License.

## Rebuild

Run the script, then render with Chrome so sizes match:

```
python3 brand/scripts/make_brand.py brand
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --hide-scrollbars --virtual-time-budget=5000 --window-size=2560,1280 --screenshot="$PWD/.github/assets/cover-light.png" "file://$PWD/brand/cover/cover-light.html"
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --hide-scrollbars --virtual-time-budget=5000 --window-size=2560,1280 --screenshot="$PWD/.github/assets/cover-dark.png" "file://$PWD/brand/cover/cover-dark.html"
```

## Usage

- Keep clear space around the mark equal to the width of the dot.
- Don't add the second dot back, and don't move the dot below the middle of the bars.
- Use the forest icon almost everywhere, and the green badge next to other Dotbrain products.
