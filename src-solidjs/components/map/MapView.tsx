import { createEffect, onCleanup, onMount } from "solid-js";
import L from "leaflet";
import "leaflet/dist/leaflet.css";

import { getGeoPaths, getGeoPoints, getSortedItems, thumbUrl, type GeoCluster } from "../../lib/ipc";
import {
  setDisplayPaths,
  setSortedItems,
  setViewMode,
} from "../../stores/galleryStore";
import { buildFilterQuery, filterQuery, ratingFilter } from "../../stores/filterStore";
import { groupBy, settings, sortField, sortOrder, subSortField, subSortOrder } from "../../stores/settingsStore";
import { openViewer } from "../../stores/viewerStore";

// Tile sources. Light = OSM standard. Dark = CartoDB Dark Matter (free,
// no API key, OSM-compatible). Both require OSM attribution; CartoDB
// additionally requires its own.
const OSM_ATTRIBUTION =
  '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors';
const TILES_LIGHT = {
  url: "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png",
  attribution: OSM_ATTRIBUTION,
  subdomains: ["a", "b", "c"],
};
const TILES_DARK = {
  url: "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png",
  attribution: `${OSM_ATTRIBUTION} &copy; <a href="https://carto.com/attributions">CARTO</a>`,
  subdomains: ["a", "b", "c", "d"],
};

const FETCH_DEBOUNCE_MS = 200;

interface MapViewProps {
  onClose?: () => void;
}

// Crossfade duration when swapping cluster sets between zoom/filter changes.
// Matches the lv-marker-fade-in keyframe in global.css.
const CROSSFADE_MS = 250;

export function MapView(_props: MapViewProps) {
  let containerEl: HTMLDivElement | undefined;
  let countEl: HTMLDivElement | undefined;
  let map: L.Map | undefined;
  let tileLayer: L.TileLayer | undefined;
  // Two-slot crossfade: `currentLayer` is the visible set; `prevLayer` is
  // the previous set fading out. When a new fetch lands while `prevLayer`
  // is still mid-fade, we drop it instantly (it's already nearly invisible)
  // before promoting the new one.
  let currentLayer: L.LayerGroup | undefined;
  let prevLayer: L.LayerGroup | undefined;
  let prevRemovalTimer: ReturnType<typeof setTimeout> | undefined;
  let fetchTimer: ReturnType<typeof setTimeout> | undefined;
  // Bumped on every viewport-change request; replies that come back after a
  // newer request started are discarded so stale clusters never overwrite
  // fresh ones.
  let fetchSeq = 0;

  const removePrevLayer = () => {
    if (prevRemovalTimer) {
      clearTimeout(prevRemovalTimer);
      prevRemovalTimer = undefined;
    }
    if (prevLayer && map) {
      map.removeLayer(prevLayer);
      prevLayer = undefined;
    }
  };

  const refreshClusters = () => {
    if (!map) return;
    const seq = ++fetchSeq;
    const b = map.getBounds();
    const zoom = Math.round(map.getZoom());
    const filter = buildFilterQuery() || undefined;

    getGeoPoints(
      {
        south: b.getSouth(),
        west: b.getWest(),
        north: b.getNorth(),
        east: b.getEast(),
      },
      zoom,
      filter,
    )
      .then((result) => {
        if (seq !== fetchSeq || !map) return;

        const newLayer = L.layerGroup();
        for (const c of result.clusters) {
          const marker = buildClusterMarker(c, () => map?.getZoom() ?? zoom);
          marker.on("click", () => handleClusterClick(c));
          newLayer.addLayer(marker);
        }
        newLayer.addTo(map);

        // Drop any layer still mid-fade-out from a prior swap; it would
        // otherwise stack visible-but-translucent under the new set.
        removePrevLayer();

        if (currentLayer) {
          prevLayer = currentLayer;
          prevLayer.eachLayer((layer) => {
            const el = (layer as L.Marker).getElement();
            if (el) {
              el.style.transition = `opacity ${CROSSFADE_MS}ms ease-out`;
              el.style.opacity = "0";
            }
          });
          // Pad the removal slightly past the transition so the final
          // frame at opacity 0 actually paints before the node detaches.
          prevRemovalTimer = setTimeout(removePrevLayer, CROSSFADE_MS + 30);
        }
        currentLayer = newLayer;

        if (countEl) {
          countEl.textContent = result.total === 1
            ? "1 photo in view"
            : `${result.total.toLocaleString()} photos in view`;
        }
      })
      .catch((e) => console.error("get_geo_points failed:", e));
  };

  const scheduleRefresh = () => {
    if (fetchTimer) clearTimeout(fetchTimer);
    fetchTimer = setTimeout(refreshClusters, FETCH_DEBOUNCE_MS);
  };

  const handleClusterClick = (c: GeoCluster) => {
    if (!map) return;
    if (c.count === 1) {
      // Singleton: open the viewer scoped to this single photo. We replace
      // displayPaths so the viewer's prev/next navigation has a coherent
      // (single-element) path list — toggling back to grid view will
      // re-fetch via the gallery's normal flow.
      setDisplayPaths([c.sample_path]);
      openViewer(0);
      return;
    }
    // Multi-photo cluster (a "stack"): collapse into the grid view scoped
    // to exactly the photos in this cluster's cell. The cell key is derived
    // from the centroid using the same floor-bucket math as the backend, so
    // the bbox we send back encloses precisely this cluster's points.
    const zoom = Math.round(map.getZoom());
    const cellDeg = (360 / Math.pow(2, Math.min(zoom, 20))) / 4;
    const keyLat = Math.floor(c.lat / cellDeg);
    const keyLon = Math.floor(c.lon / cellDeg);
    const bbox = {
      south: keyLat * cellDeg,
      north: (keyLat + 1) * cellDeg,
      west: keyLon * cellDeg,
      east: (keyLon + 1) * cellDeg,
    };
    const filter = buildFilterQuery() || undefined;
    getGeoPaths(bbox, filter)
      .then(async (paths) => {
        if (paths.length === 0) return;
        const sorted = await getSortedItems(
          sortField(),
          sortOrder(),
          groupBy(),
          paths,
          subSortField(),
          subSortOrder(),
        );
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
        setViewMode("grid");
      })
      .catch((e) => console.error("get_geo_paths failed:", e));
  };

  onMount(() => {
    if (!containerEl) return;
    map = L.map(containerEl, {
      worldCopyJump: true,
      // Zoom animation keeps the previous zoom's tiles scaled in place
      // until the new tiles finish loading, crossfading the LODs so the
      // map doesn't blank during the network fetch.
      zoomAnimation: true,
    }).setView([20, 0], 2);

    map.on("moveend", scheduleRefresh);
    map.on("zoomend", scheduleRefresh);

    // Swap tile source when the dark-mode toggle flips. Recreating the
    // layer (rather than setUrl) is needed because the attribution differs
    // between providers. Nested inside onMount so `map` is guaranteed to
    // exist when the effect first runs.
    createEffect(() => {
      const dark = settings().display.map_dark_mode;
      if (!map) return;
      const cfg = dark ? TILES_DARK : TILES_LIGHT;
      if (tileLayer) map.removeLayer(tileLayer);
      tileLayer = L.tileLayer(cfg.url, {
        attribution: cfg.attribution,
        maxZoom: 19,
        subdomains: cfg.subdomains,
      });
      tileLayer.addTo(map);
      if (typeof tileLayer.bringToBack === "function") tileLayer.bringToBack();
    });

    // Initial fetch on mount.
    scheduleRefresh();
  });

  // React to filter-bar changes — same viewport, different predicate.
  createEffect(() => {
    filterQuery();
    ratingFilter();
    if (map) scheduleRefresh();
  });

  onCleanup(() => {
    if (fetchTimer) clearTimeout(fetchTimer);
    if (map) {
      map.remove();
      map = undefined;
    }
  });

  return (
    <div class="absolute inset-0 z-10">
      <div ref={containerEl} class="w-full h-full" style={{ background: "#0a0a0a" }} />
      <div
        ref={countEl}
        class="absolute bottom-3 right-3 z-[500] px-2.5 py-1 rounded text-xs text-neutral-200 pointer-events-none"
        style={{
          background: "rgba(10, 10, 10, 0.85)",
          "backdrop-filter": "blur(8px)",
        }}
      >
        Loading…
      </div>
    </div>
  );
}

/// Render a cluster marker as an HTML divIcon so we can show a thumbnail
/// for singletons and a count badge for multi-photo clusters without
/// shipping any image-specific Leaflet plugins.
function buildClusterMarker(c: GeoCluster, _currentZoom: () => number): L.Marker {
  const html =
    c.count === 1
      ? `
        <div class="lv-marker lv-marker-single">
          <img src="${thumbUrl(c.sample_path, "s")}" alt="" />
        </div>
      `
      : `
        <div class="lv-marker lv-marker-cluster">
          <img src="${thumbUrl(c.sample_path, "s")}" alt="" />
          <span class="lv-marker-badge">${formatCount(c.count)}</span>
        </div>
      `;

  const icon = L.divIcon({
    html,
    className: "",
    iconSize: c.count === 1 ? [40, 40] : [44, 44],
    iconAnchor: c.count === 1 ? [20, 20] : [22, 22],
  });
  return L.marker([c.lat, c.lon], { icon });
}

function formatCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 10_000) return `${(n / 1000).toFixed(1)}k`;
  return `${Math.round(n / 1000)}k`;
}
