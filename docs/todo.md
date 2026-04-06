
- Add 'recently viewed' and 'date added to gallery' tracking and sort options.

- Add 'recently rated' tracking and sort option. Additionally, allow number keys to be used to rate, if an image is focused hitting '3' will rate it '3 stars'

- Add subfilters ie. primarily sort by rating, then within ratings sort by date.

- Move the Star filter UI to the other side of the filter bar next to the sort buttons.

- While typing in the tag bar of the info panel, the 'i' key should not close the info panel.

- Add image zooming (ctrl+scroll) and current pixel ratio in the info menu (ie. a 1000x1000 image that takes up exactly 1000x1000 pixels on screen shows 1:1) This will require Lightview to know the resolution of the screen its on.

- Prevent scrolling while an image is opened. If an image is zoomed to larger than the viewport scrolling and sidescrolling should pan the image.

- ctrl + scroll should increase/decrease the thumbnail sizes when an image is not opened.

- Support AVIF thumbnail generation

- When using arrow keys to navigate between images if the image opened is not currently in the grid scroll the grid to include the image currently being viewed.

- Improve scroll bar date label placement, larger labels for start of each year, smaller labels for each month?

- Abstract ALL constants into a defined file, settings should be able to control some of these

- Make debug menu properly show number of cached images

- If an image is deleted from a directory by the user or another program the thumbnail shows up in the UI, and clicking fails to load. Need some mechanism to verify the underlying image actually still exists without impacting performance.

- Webserver for webview, for viewing from phone or other devices. We want to keep the performance benefit of Tauri when running on the main device but want a 'headless' backend that consumes as few resources as possible when not actively in use, so it can be made to startup on boot with negligable performance impact.

- Add virtual folder view, default hierarchy should be plugin name or user folders at the top and a folder for each tag inside. The folder view is entirely virtual and does not actually copy or move images. 

- Improve scroll performance
