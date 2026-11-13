import importlib.util, unittest
from unittest.mock import patch
from pathlib import Path
path = Path(__file__).resolve().parents[2] / 'src/assets/region_init.py'
spec = importlib.util.spec_from_file_location('region_init', path)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class RegionGeometryTests(unittest.TestCase):

    def test_missing_column_requires_geometry_repair(self):
        with patch.object(m, 'has_geometry_column', return_value=False), patch.object(m, 'psql') as sql:
            self.assertTrue(m.needs_geometry_repair([], 'db', 'public', 'sys_region'))
            sql.assert_not_called()

    def test_null_geometry_requires_repair(self):
        with patch.object(m, 'has_geometry_column', return_value=True), patch.object(m, 'psql', return_value='t'):
            self.assertTrue(m.needs_geometry_repair([], 'db', 'public', 'sys_region'))

    def test_complete_geometry_does_not_require_repair(self):
        with patch.object(m, 'has_geometry_column', return_value=True), patch.object(m, 'psql', return_value='f'):
            self.assertFalse(m.needs_geometry_repair([], 'db', 'public', 'sys_region'))

    def test_status_does_not_mislabel_missing_geometry_as_only_missing_index(self):
        with patch.object(m, 'inspect'), patch.object(m, 'target', return_value=[]), patch.object(m, 'pending_targets', return_value=([], list(m.TARGETS))), patch.object(m, 'needs_geometry_repair', side_effect=[False, True]), patch.object(m, 'has_spatial_index', return_value=True):
            status = m.check('unused')
            self.assertEqual(['cl-base.public.sys_region'], status['geometryRepairs'])
            self.assertEqual([], status['indexRepairs'])
            self.assertFalse(status['initialized'])

    def test_index_only_run_never_reimports_or_repairs_geometry(self):
        with patch.object(m, 'inspect', return_value='0' * 64), patch.object(m, 'target', return_value=[]), patch.object(m, 'pending_targets', return_value=([], list(m.TARGETS))), patch.object(m, 'needs_geometry_repair', return_value=False), patch.object(m, 'ensure_spatial_index', return_value='ready'), patch.object(m, 'import_one') as imp, patch.object(m, 'repair_geometry') as repair:
            m.run('unused')
            imp.assert_not_called()
            repair.assert_not_called()
if __name__ == '__main__':
    unittest.main()
