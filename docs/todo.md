
- When zooming out on the grid, thumbnails don't show until a scroll command is input. Preferably we would attach as little as possible to the scoll input. This could be a source of performance issues with scrolling. Research high performance methods for scrolling the grid view. Try to find out how top tier gallery apps like Apple photos, google photos, and Immich handle this. Some prior research exists in docs/scrollPerformanceResearch.md

- improve scroll bar visibility

- Optimize HEIC loading pipeline

- Move the Star filter UI to the other side of the filter bar next to the sort buttons.

- Regenerating thumbnails needs to clear the previous thumbnail and replace it. Right now to regenerate a thumbnail the user must click regenerate and then restart the program for it to take effect

- Support AVIF thumbnail generation

- Improve scroll bar date label placement, larger labels for start of each year, smaller labels for each month?
  
- Make debug menu properly show number of full images currently cached

- Webserver for webview, for viewing from phone or other devices. We want to keep the performance benefit of Tauri when running on the main device but want a 'headless' backend that consumes as few resources as possible when not actively in use, so it can be made to startup on boot with negligable performance impact.

- Add virtual folder view, default hierarchy should be plugin name and user folders at the top and a folder for each tag inside. The folder view is entirely virtual and does not actually copy or move images. 

- Improve scroll performance
