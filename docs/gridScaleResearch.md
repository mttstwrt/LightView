Modern photo apps like Apple Photos and Google Photos use a multi-tiered caching strategy and dynamic tiling to keep the grid fluid. They don't just "stretch" a single small image; they treat the photo library more like a map (similar to Google Maps) where zooming in and out triggers different "Level of Detail" (LOD) fetches.

### 1. The Thumbnail Pyramid (LOD)
Instead of one thumbnail, the system generates a "pyramid" of versions for every photo upon import or upload.
* **Micro/Tiny:** ~50–100px. Used for the "Years" or "Months" views where hundreds of photos are on screen.
* **Small/Medium:** ~250–500px. Used for the standard day-to-day grid.
* **Full-Screen Preview:** High-quality but compressed (usually screen resolution).
* **Original:** The raw file, only fetched when you zoom in past 1:1 or hit "Edit."

### 2. Transition Mechanics: Cross-Fading
When you pinch-to-zoom (changing the grid size), the app doesn't immediately swap the image file.
* **The "Interpolation" Phase:** As you pinch, the app takes the currently visible thumbnail and scales it using the GPU. If you are zooming in, the small thumbnail will look slightly blurry for a split second.
* **The "Swap" Phase:** Once the pinch gesture ends (or hits a certain threshold), the app requests the next size up from the cache. Once that higher-res image is ready, it **cross-fades** it over the blurry one so the "pop" into clarity feels smooth rather than jarring.

### 3. Smart Prefetching & Viewport Logic
Apps use a **"Prefetch Window"** to anticipate your next move.
* **Buffer Zones:** The app renders and fetches thumbnails for images about 1–2 screens above and below your current scroll position.
* **Priority Queuing:** If you are zooming in, the app prioritizes the images currently in the center of your pinch gesture, as those are the most likely to be inspected first.

### 4. Apple-Specific: `PHImageManager` & `UICollectionView`
In the Apple ecosystem, developers use the **Photos Framework**. 
* **`requestImage(for:targetSize:contentMode:options:resultHandler:)`**: This function is the "magic" behind the scaling. Apple’s underlying engine decides whether to give you a fast, low-quality version first or wait for a high-quality one based on the `targetSize` you provide.
* **`preparingThumbnail(of:)`**: A modern iOS API that handles the downsampling off the main thread, preventing the "stutter" that happens when decoding large images into small grid cells.

### 5. Google-Specific: WebP and Dynamic URLs
Since Google Photos is cloud-first, they use a dynamic URL-based approach.
* **Dynamic Resizing:** Google’s image servers can resize images on the fly. A thumbnail URL might look like `.../image-id=w400-h400-c`, where the `w400` tells the server to crop and serve exactly a 400px image.
* **WebP/AVIF:** They use highly efficient formats that allow for very small file sizes with high visual fidelity, making the "stream" feel instant even on cellular data.

### Summary of Techniques
| Feature | Implementation |
| :--- | :--- |
| **Pinch-to-Zoom** | GPU-accelerated scaling of the existing texture + cross-fade. |
| **Grid Resizing** | Swapping between different tiers of the "Thumbnail Pyramid." |
| **Smooth Scrolling** | Off-thread decoding and prefetching into a memory cache. |
| **Data Efficiency** | Serving different resolutions based on the physical pixel density of the grid cell. |
