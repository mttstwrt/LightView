
- Gif play on hover

- If an image is added to or removed from the directory by the user or another program the UI should react accordingly

- Add deduplication menu in settings, shows duplicates, for images that are similar but not exactly the same highlight a green box around the 'best' version (base solely off of highest resolution, or in the case of identical resolutions the smaller file size)

- Move the Star filter UI to the other side of the filter bar next to the sort buttons.

- Prevent scrolling while an image is opened. If an image is zoomed to larger than the viewport scrolling and sidescrolling should pan the image.

- ctrl + scroll should increase/decrease the thumbnail sizes when an image is not opened.

- Support AVIF thumbnail generation

- When using arrow keys to navigate between images if the image opened is not currently in the grid scroll the grid to include the image currently being viewed.

- Improve scroll bar date label placement, larger labels for start of each year, smaller labels for each month?

- Abstract ALL constants into a defined file, settings should be able to control some of these
  
- Make debug menu properly show number of cached images

- If an image is deleted from a directory by the user or another program the thumbnail shows up in the UI, and clicking fails to load. Need some mechanism to verify the underlying image actually still exists without impacting performance.

- Webserver for webview, for viewing from phone or other devices. We want to keep the performance benefit of Tauri when running on the main device but want a 'headless' backend that consumes as few resources as possible when not actively in use, so it can be made to startup on boot with negligable performance impact.

- Add virtual folder view, default hierarchy should be plugin name and user folders at the top and a folder for each tag inside. The folder view is entirely virtual and does not actually copy or move images. 

- Improve scroll performance
