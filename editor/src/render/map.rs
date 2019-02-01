use crate::colors::ColorScheme;
use crate::objects::ID;
use crate::render::area::DrawArea;
use crate::render::building::DrawBuilding;
use crate::render::bus_stop::DrawBusStop;
use crate::render::extra_shape::{DrawExtraShape, ExtraShapeID};
use crate::render::intersection::DrawIntersection;
use crate::render::lane::DrawLane;
use crate::render::parcel::DrawParcel;
use crate::render::pedestrian::DrawPedestrian;
use crate::render::turn::DrawTurn;
use crate::render::{draw_vehicle, Renderable};
use crate::state::{Flags, ShowObjects};
use aabb_quadtree::QuadTree;
use abstutil::Timer;
use ezgui::Prerender;
use geom::Bounds;
use map_model::{
    AreaID, BuildingID, BusStopID, FindClosest, IntersectionID, Lane, LaneID, Map, ParcelID,
    RoadID, Traversable, Turn, TurnID, LANE_THICKNESS,
};
use sim::{GetDrawAgents, Tick};
use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(PartialEq)]
pub enum RenderOrder {
    BackToFront,
    FrontToBack,
}

pub struct DrawMap {
    pub lanes: Vec<DrawLane>,
    pub intersections: Vec<DrawIntersection>,
    pub turns: HashMap<TurnID, DrawTurn>,
    pub buildings: Vec<DrawBuilding>,
    pub parcels: Vec<DrawParcel>,
    pub extra_shapes: Vec<DrawExtraShape>,
    pub bus_stops: HashMap<BusStopID, DrawBusStop>,
    pub areas: Vec<DrawArea>,

    agents: RefCell<AgentCache>,

    quadtree: QuadTree<ID>,
}

impl DrawMap {
    pub fn new(
        map: &Map,
        flags: &Flags,
        cs: &ColorScheme,
        prerender: &Prerender,
        timer: &mut Timer,
    ) -> DrawMap {
        timer.start_iter("make DrawLanes", map.all_lanes().len());
        let mut lanes: Vec<DrawLane> = Vec::new();
        for l in map.all_lanes() {
            timer.next();
            lanes.push(DrawLane::new(l, map, cs, prerender));
        }

        let mut turn_to_lane_offset: HashMap<TurnID, usize> = HashMap::new();
        for l in map.all_lanes() {
            DrawMap::compute_turn_to_lane_offset(&mut turn_to_lane_offset, l, map);
        }
        assert_eq!(turn_to_lane_offset.len(), map.all_turns().len());

        timer.start_iter("make DrawTurns", map.all_turns().len());
        let mut turns: HashMap<TurnID, DrawTurn> = HashMap::new();
        for t in map.all_turns().values() {
            timer.next();
            turns.insert(t.id, DrawTurn::new(map, t, turn_to_lane_offset[&t.id]));
        }

        timer.start_iter("make DrawIntersections", map.all_intersections().len());
        let intersections: Vec<DrawIntersection> = map
            .all_intersections()
            .iter()
            .map(|i| {
                timer.next();
                DrawIntersection::new(i, map, cs, prerender)
            })
            .collect();

        timer.start_iter("make DrawBuildings", map.all_buildings().len());
        let buildings: Vec<DrawBuilding> = map
            .all_buildings()
            .iter()
            .map(|b| {
                timer.next();
                DrawBuilding::new(b, cs, prerender)
            })
            .collect();

        let mut parcels: Vec<DrawParcel> = Vec::new();
        if flags.draw_parcels {
            timer.start_iter("make DrawParcels", map.all_parcels().len());
            for p in map.all_parcels() {
                timer.next();
                parcels.push(DrawParcel::new(p, cs, prerender));
            }
        }

        let mut extra_shapes: Vec<DrawExtraShape> = Vec::new();
        if let Some(ref path) = flags.kml {
            let raw_shapes = if path.ends_with(".kml") {
                kml::load(&path, &map.get_gps_bounds(), timer)
                    .expect("Couldn't load extra KML shapes")
                    .shapes
            } else {
                let shapes: kml::ExtraShapes =
                    abstutil::read_binary(&path, timer).expect("Couldn't load ExtraShapes");
                shapes.shapes
            };

            // Match shapes with the nearest road + direction (true for forwards)
            let mut closest: FindClosest<(RoadID, bool)> =
                map_model::FindClosest::new(&map.get_bounds());
            for r in map.all_roads().iter() {
                closest.add((r.id, true), &r.center_pts.shift_right(LANE_THICKNESS));
                closest.add((r.id, false), &r.center_pts.shift_left(LANE_THICKNESS));
            }

            let gps_bounds = map.get_gps_bounds();
            for s in raw_shapes.into_iter() {
                if let Some(es) =
                    DrawExtraShape::new(ExtraShapeID(extra_shapes.len()), s, gps_bounds, &closest)
                {
                    extra_shapes.push(es);
                }
            }
        }

        let mut bus_stops: HashMap<BusStopID, DrawBusStop> = HashMap::new();
        for s in map.all_bus_stops().values() {
            bus_stops.insert(s.id, DrawBusStop::new(s, map));
        }
        let areas: Vec<DrawArea> = map.all_areas().iter().map(|a| DrawArea::new(a)).collect();

        timer.start("create quadtree");
        let mut quadtree = QuadTree::default(map.get_bounds().as_bbox());
        // TODO use iter chain if everything was boxed as a renderable...
        for obj in &lanes {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        for obj in &intersections {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        for obj in &buildings {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        for obj in &parcels {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        for obj in &extra_shapes {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        for obj in bus_stops.values() {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        for obj in &areas {
            quadtree.insert_with_box(obj.get_id(), obj.get_bounds().as_bbox());
        }
        timer.stop("create quadtree");

        DrawMap {
            lanes,
            intersections,
            turns,
            buildings,
            parcels,
            extra_shapes,
            bus_stops,
            areas,

            agents: RefCell::new(AgentCache {
                tick: None,
                agents_per_on: HashMap::new(),
            }),

            quadtree,
        }
    }

    fn compute_turn_to_lane_offset(result: &mut HashMap<TurnID, usize>, l: &Lane, map: &Map) {
        // Split into two groups, based on the endpoint
        let mut pair: (Vec<&Turn>, Vec<&Turn>) = map
            .get_turns_from_lane(l.id)
            .iter()
            .partition(|t| t.id.parent == l.dst_i);

        // Sort the turn icons by angle.
        pair.0
            .sort_by_key(|t| t.angle().normalized_degrees() as i64);
        pair.1
            .sort_by_key(|t| t.angle().normalized_degrees() as i64);

        for (idx, t) in pair.0.iter().enumerate() {
            result.insert(t.id, idx);
        }
        for (idx, t) in pair.1.iter().enumerate() {
            result.insert(t.id, idx);
        }
    }

    pub fn edit_lane_type(
        &mut self,
        id: LaneID,
        map: &Map,
        cs: &ColorScheme,
        prerender: &Prerender,
    ) {
        // No need to edit the quadtree; the bbox shouldn't depend on lane type.
        self.lanes[id.0] = DrawLane::new(map.get_l(id), map, cs, prerender);
    }

    pub fn edit_remove_turn(&mut self, id: TurnID) {
        self.turns.remove(&id);
    }

    pub fn edit_add_turn(&mut self, id: TurnID, map: &Map) {
        let t = map.get_t(id);
        let mut turn_to_lane_offset: HashMap<TurnID, usize> = HashMap::new();
        DrawMap::compute_turn_to_lane_offset(&mut turn_to_lane_offset, map.get_l(id.src), map);
        let draw_turn = DrawTurn::new(map, t, turn_to_lane_offset[&id]);
        self.turns.insert(id, draw_turn);
    }

    // The alt to these is implementing std::ops::Index, but that's way more verbose!
    pub fn get_l(&self, id: LaneID) -> &DrawLane {
        &self.lanes[id.0]
    }

    pub fn get_i(&self, id: IntersectionID) -> &DrawIntersection {
        &self.intersections[id.0]
    }

    pub fn get_t(&self, id: TurnID) -> &DrawTurn {
        &self.turns[&id]
    }

    pub fn get_b(&self, id: BuildingID) -> &DrawBuilding {
        &self.buildings[id.0]
    }

    pub fn get_p(&self, id: ParcelID) -> &DrawParcel {
        &self.parcels[id.0]
    }

    pub fn get_es(&self, id: ExtraShapeID) -> &DrawExtraShape {
        &self.extra_shapes[id.0]
    }

    pub fn get_bs(&self, id: BusStopID) -> &DrawBusStop {
        &self.bus_stops[&id]
    }

    pub fn get_a(&self, id: AreaID) -> &DrawArea {
        &self.areas[id.0]
    }

    #[allow(dead_code)]
    pub fn get_matching_lanes(&self, bounds: Bounds) -> Vec<LaneID> {
        let mut results: Vec<LaneID> = Vec::new();
        for &(id, _, _) in &self.quadtree.query(bounds.as_bbox()) {
            if let ID::Lane(id) = id {
                results.push(*id);
            }
        }
        results
    }

    // False from the callback means abort
    pub fn handle_objects<T: ShowObjects, F: FnMut(Box<&Renderable>) -> bool>(
        &self,
        screen_bounds: Bounds,
        map: &Map,
        sim: &GetDrawAgents,
        show_objs: &T,
        order: RenderOrder,
        mut callback: F,
    ) {
        // From background to foreground Z-order
        let mut areas: Vec<Box<&Renderable>> = Vec::new();
        let mut parcels: Vec<Box<&Renderable>> = Vec::new();
        let mut lanes: Vec<Box<&Renderable>> = Vec::new();
        let mut intersections: Vec<Box<&Renderable>> = Vec::new();
        let mut buildings: Vec<Box<&Renderable>> = Vec::new();
        let mut extra_shapes: Vec<Box<&Renderable>> = Vec::new();
        let mut bus_stops: Vec<Box<&Renderable>> = Vec::new();
        let mut turn_icons: Vec<Box<&Renderable>> = Vec::new();
        // We can't immediately grab the Renderables; we need to do it all at once at the end.
        let mut agents_on: Vec<Traversable> = Vec::new();

        for &(id, _, _) in &self.quadtree.query(screen_bounds.as_bbox()) {
            if show_objs.show(*id) {
                match id {
                    ID::Area(id) => areas.push(Box::new(self.get_a(*id))),
                    ID::Parcel(id) => parcels.push(Box::new(self.get_p(*id))),
                    ID::Lane(id) => {
                        lanes.push(Box::new(self.get_l(*id)));
                        if !show_objs.show_icons_for(map.get_l(*id).dst_i) {
                            let on = Traversable::Lane(*id);
                            if !self.agents.borrow().has(sim.tick(), on) {
                                let mut list: Vec<Box<Renderable>> = Vec::new();
                                for c in sim.get_draw_cars(on, map).into_iter() {
                                    list.push(draw_vehicle(c, map));
                                }
                                for p in sim.get_draw_peds(on, map).into_iter() {
                                    list.push(Box::new(DrawPedestrian::new(p, map)));
                                }
                                self.agents.borrow_mut().put(sim.tick(), on, list);
                            }
                            agents_on.push(on);
                        }
                    }
                    ID::Intersection(id) => {
                        intersections.push(Box::new(self.get_i(*id)));
                        for t in &map.get_i(*id).turns {
                            if show_objs.show_icons_for(*id) {
                                turn_icons.push(Box::new(self.get_t(*t)));
                            } else {
                                let on = Traversable::Turn(*t);
                                if !self.agents.borrow().has(sim.tick(), on) {
                                    let mut list: Vec<Box<Renderable>> = Vec::new();
                                    for c in sim.get_draw_cars(on, map).into_iter() {
                                        list.push(draw_vehicle(c, map));
                                    }
                                    for p in sim.get_draw_peds(on, map).into_iter() {
                                        list.push(Box::new(DrawPedestrian::new(p, map)));
                                    }
                                    self.agents.borrow_mut().put(sim.tick(), on, list);
                                }
                                agents_on.push(on);
                            }
                        }
                    }
                    // TODO front paths will get drawn over buildings, depending on quadtree order.
                    // probably just need to make them go around other buildings instead of having
                    // two passes through buildings.
                    ID::Building(id) => buildings.push(Box::new(self.get_b(*id))),
                    ID::ExtraShape(id) => extra_shapes.push(Box::new(self.get_es(*id))),
                    ID::BusStop(id) => bus_stops.push(Box::new(self.get_bs(*id))),

                    ID::Turn(_) | ID::Car(_) | ID::Pedestrian(_) | ID::Trip(_) => {
                        panic!("{:?} shouldn't be in the quadtree", id)
                    }
                }
            }
        }

        let mut borrows: Vec<Box<&Renderable>> = Vec::new();
        borrows.extend(areas);
        borrows.extend(parcels);
        borrows.extend(lanes);
        borrows.extend(intersections);
        borrows.extend(buildings);
        borrows.extend(extra_shapes);
        borrows.extend(bus_stops);
        borrows.extend(turn_icons);
        let cache = self.agents.borrow();
        for on in agents_on {
            for obj in cache.get(on) {
                borrows.push(obj);
            }
        }

        // This is a stable sort.
        borrows.sort_by_key(|r| r.get_zorder());

        if order == RenderOrder::FrontToBack {
            borrows.reverse();
        }

        for r in borrows {
            if !callback(r) {
                break;
            }
        }
    }
}

struct AgentCache {
    tick: Option<Tick>,
    agents_per_on: HashMap<Traversable, Vec<Box<Renderable>>>,
}

impl AgentCache {
    fn has(&self, tick: Tick, on: Traversable) -> bool {
        if Some(tick) != self.tick {
            return false;
        }
        self.agents_per_on.contains_key(&on)
    }

    // Must call has() first.
    fn get(&self, on: Traversable) -> Vec<Box<&Renderable>> {
        self.agents_per_on[&on]
            .iter()
            .map(|obj| Box::new(obj.borrow()))
            .collect()
    }

    fn put(&mut self, tick: Tick, on: Traversable, agents: Vec<Box<Renderable>>) {
        if Some(tick) != self.tick {
            self.agents_per_on.clear();
            self.tick = Some(tick);
        }

        assert!(!self.agents_per_on.contains_key(&on));
        self.agents_per_on.insert(on, agents);
    }
}
