
- Gif play on hover

- If an image is added to or removed from the directory by the user or another program the UI should update in real time, with as little performance impact as possible

- improve scroll bar visibility

- Move the Star filter UI to the other side of the filter bar next to the sort buttons.

- ctrl + scroll should increase/decrease the thumbnail sizes when an image is not opened.

- Support AVIF thumbnail generation

- When using arrow keys to navigate between images if the image opened is not currently in the grid scroll the grid to include the image currently being viewed.

- Improve scroll bar date label placement, larger labels for start of each year, smaller labels for each month?

- Abstract ALL constants into a defined file, settings should be able to control some of these
  
- Make debug menu properly show number of full images currently cached

- Webserver for webview, for viewing from phone or other devices. We want to keep the performance benefit of Tauri when running on the main device but want a 'headless' backend that consumes as few resources as possible when not actively in use, so it can be made to startup on boot with negligable performance impact.

- Add virtual folder view, default hierarchy should be plugin name and user folders at the top and a folder for each tag inside. The folder view is entirely virtual and does not actually copy or move images. 

- Regenerating thumbnails needs to clear the previous thumbnail and replace it. Right now to regenerate a thumbnail the user must click regenerate and then restart the program for it to take effect

- Improve scroll performance
