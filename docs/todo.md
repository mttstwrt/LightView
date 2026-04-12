
- make thumbnail info in the image info panel an expandable section, showing all levels of thumbnail generated for that image, the resolution of each thumbnail level, file size, the date it was generated, with what algorithm, and what file format.

- make ctrl+f open the filter bar even when not moused over the top bar

- make 'I' hotkey open the settings panel if in grid view

- make 'f11' toggle full screen

- in the info panel add 'date rated' next to the rating. If there is a rating but no date it should say 'unknown' and blank if there is no rating at all.

- Thumbnail streaming? How do Apple photos/Google photos handle small vs large thumbnails gracefully? When Thumbnails are big we want higher res and we don't need to worry about performance as much because of fewer images and fewer dom nodes. When thumbnails are small we want to load lower res and figure out how to handle the increased number of DOM nodes. Research high performance methods of storing/loading thumbnails for the grid view. Try to find out how top tier gallery apps like Apple photos, google photos, and Immich handle this. Some prior research exists in docs/gridScaleResearch.md

- When zooming out on the grid, unloaded thumbnails show until a scroll command is input. Preferably we would attach as little as possible to the scoll input. This could be a source of performance issues with scrolling. Research high performance methods for scrolling the grid view. Try to find out how top tier gallery apps like Apple photos, google photos, and Immich handle this. Some prior research exists in docs/scrollPerformanceResearch.md

- When zooming in and out on the grid, try to keep the hovered image in view.

- improve scroll bar visibility

- Optimize HEIC loading pipeline

- Move the Star filter UI to the other side of the filter bar next to the sort buttons.

- Regenerating thumbnails needs to clear the previous thumbnail and replace it. Right now to regenerate a thumbnail the user must click regenerate and then restart the program for it to take effect

- ctrl + scroll should increase/decrease the thumbnail sizes when an image is not opened.

- Support AVIF thumbnail generation

- When using arrow keys to navigate between images if the image opened is not currently in the grid scroll the grid to include the image currently being viewed.

- Improve scroll bar date label placement, larger labels for start of each year, smaller labels for each month?

- Abstract ALL constants into a defined file, settings should be able to control some of these
  
- Make debug menu properly show number of full images currently cached

- Webserver for webview, for viewing from phone or other devices. We want to keep the performance benefit of Tauri when running on the main device but want a 'headless' backend that consumes as few resources as possible when not actively in use, so it can be made to startup on boot with negligable performance impact.

- Add virtual folder view, default hierarchy should be plugin name and user folders at the top and a folder for each tag inside. The folder view is entirely virtual and does not actually copy or move images. 


- Improve scroll performance
