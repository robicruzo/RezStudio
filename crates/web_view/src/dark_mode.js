// Darkens light pages to suit a dark theme, leaving pages that are already
// dark alone. REZSTUDIO_CONTRAST is replaced by the contrast the inverted
// page is drawn at, so white becomes about the theme's background rather
// than black, or by null to stop darkening.
(() => {
  const contrast = REZSTUDIO_CONTRAST;
  const darkClass = "rezstudio-dark";
  const styleId = "rezstudio-dark-mode";
  // Inverting and turning hues back keeps colors' hues. Photos are turned
  // back again so they don't look like negatives; PNG and SVG images stay
  // darkened, since in R output they are mostly plots.
  const css = `html.${darkClass} {
  filter: invert(1) hue-rotate(180deg) contrast(${contrast});
}
html.${darkClass} :is(
  img[src*=".jpg" i], img[src*=".jpeg" i], img[src*=".webp" i],
  img[src^="data:image/jpeg" i], img[src^="data:image/webp" i], video
) {
  filter: hue-rotate(180deg) invert(1);
}`;

  const parseColor = (color) => {
    const match = color.match(/rgba?\(([^)]*)\)/);
    if (!match) {
      return null;
    }
    const [red, green, blue, alpha = 1] = match[1]
      .split(/[\s,/]+/)
      .filter((part) => part !== "")
      .map(Number);
    return { red, green, blue, alpha };
  };
  const luminance = ({ red, green, blue }) =>
    (0.2126 * red + 0.7152 * green + 0.0722 * blue) / 255;

  // A page is dark when its own background is. Without one, its text color
  // tells: a dark color scheme, from CSS or a `<meta name="color-scheme">`,
  // makes the default text light. A filter doesn't change computed styles,
  // so this measures the page's own colors even while it's darkened.
  const isDark = () => {
    for (const element of [document.body, document.documentElement]) {
      if (!element) {
        continue;
      }
      const background = parseColor(getComputedStyle(element).backgroundColor);
      if (background && background.alpha > 0.5) {
        return luminance(background) < 0.5;
      }
    }
    const text = parseColor(getComputedStyle(document.body ?? document.documentElement).color);
    return text !== null && luminance(text) > 0.5;
  };

  // Each toggle adds another copy of this script, and they run in order, so
  // the newest decides.
  const apply = (checkPage) => {
    const root = document.documentElement;
    if (!root) {
      return;
    }
    let style = document.getElementById(styleId);
    if (contrast === null) {
      root.classList.remove(darkClass);
      style?.remove();
      return;
    }
    if (!style) {
      style = document.createElement("style");
      style.id = styleId;
      root.prepend(style);
    }
    style.textContent = css;
    root.classList.toggle(darkClass, !checkPage || !isDark());
  };

  // Most R output is light, so a page is darkened as soon as it starts, before
  // it draws, and checked once its styles and scripts have run.
  apply(false);
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", () => apply(true), { once: true });
  } else {
    apply(true);
  }
  window.addEventListener("load", () => apply(true), { once: true });
})();
