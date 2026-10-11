#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""The page drain rejects transport defects before grading semantic evidence."""
import copy
import json
import unittest

from trace_pages import drain_impact, drain_references, drain_trace


def wire(page):
    page = copy.deepcopy(page)
    accounting = page['_kin'].setdefault('response', {})
    accounting.update(max_chars=2000, chars_after_budget=0)
    for _ in range(8):
        encoded = json.dumps(page, separators=(',', ':'), ensure_ascii=False)
        size = len(encoded.encode('utf-8'))
        if accounting['chars_after_budget'] == size:
            return page, encoded
        accounting['chars_after_budget'] = size
    raise AssertionError('wire byte counter did not settle')


def sample():
    body = 'λ"\\\n' * 6
    base = {
        'chain': [], 'next_cursor': None,
        '_kin': {'page': {'version': 1, 'complete': False, 'total_steps': 1},
                 'verdict': {'state': 'inconclusive', 'safe_to_conclude_absent': False},
                 'completeness': {'status': 'partial', 'limits': ['trace_page_partial']}},
        'negative': {'safe_to_conclude_absent': False},
    }
    pages = []
    for index, text in enumerate([body[:12], body[12:]]):
        page = copy.deepcopy(base)
        page['next_cursor'] = 'page-%d' % (index + 1)
        page['record_fragment'] = {
            'collection': 'readings', 'index': 0, 'key': 'focal_entity',
            'entity_id': 'one', 'step': 0, 'parent_step': 0,
            'field': 'body', 'encoding': 'utf8', 'byte_offset': 0 if index == 0 else len(body[:12].encode('utf-8')),
            'total_bytes': len(body.encode('utf-8')), 'field_complete': index == 1,
            'record_complete': False, 'text': text,
        }
        pages.append(page)
    page = copy.deepcopy(base)
    page['chain'] = [{'step': 1, 'parent_step': 0, 'entity_id': 'child'}]
    page['record_fragment'] = {
        'collection': 'readings', 'index': 0, 'key': 'focal_entity',
        'entity_id': 'one', 'step': 0, 'parent_step': 0,
        'field': 'id', 'encoding': 'utf8', 'byte_offset': 0,
        'total_bytes': 3, 'field_complete': True, 'record_complete': True, 'text': 'one',
    }
    pages.append(page)
    return pages, {'chain': page['chain'], 'focal_entity': {'body': body, 'id': 'one'}}


def drain(pages):
    # A real server resumes the requested cursor, not the next fixture row.
    # Repeating a cursor must repeat its page so a stalled drain is visible.
    by_cursor = {None: pages[0]}
    by_cursor.update((previous['next_cursor'], following)
                     for previous, following in zip(pages, pages[1:]))
    return drain_trace(lambda cursor: wire(by_cursor[cursor]), 2000)


class TracePagesTest(unittest.TestCase):
    def impact_pages(self):
        base = {
            'affected_callers': [], 'next_cursor': 'second',
            '_kin': {'page': {'version': 1, 'kind': 'impact', 'complete': False,
                              'total_rows': 3, 'returned_rows': 1},
                     'verdict': {'state': 'inconclusive', 'safe_to_conclude_absent': False},
                     'completeness': {'status': 'partial', 'limits': ['impact_page_partial']}},
            'negative': {'safe_to_conclude_absent': False},
        }
        first = copy.deepcopy(base)
        first['affected_callers'] = [{'id': 'caller', 'body': 'λ'}]
        second = copy.deepcopy(base)
        second['next_cursor'] = None
        second['_kin']['page']['returned_rows'] = 2
        second['affected_overrides'] = [{'id': 'override'}]
        second['affected_tests'] = [{'id': 'test'}]
        second['readings'] = [
            {'key': 'changed_ids', 'value': ['changed']},
            {'key': '_kin', 'value': {'verdict': {'state': 'inconclusive',
                                                 'limiting_factor': 'reference_completion'}}},
            {'key': 'negative', 'value': {'safe_to_conclude_absent': False,
                                          'reason': 'unmeasured references'}},
        ]
        return [first, second]

    def drain_impact_pages(self, pages):
        by_cursor = {None: pages[0], 'second': pages[1]}
        return drain_impact(lambda cursor: wire(by_cursor[cursor]), 2000)

    def test_impact_pages_preserve_all_buckets_and_readings(self):
        answer = self.drain_impact_pages(self.impact_pages())
        self.assertEqual(answer['affected_callers'], [{'id': 'caller', 'body': 'λ'}])
        self.assertEqual(answer['affected_overrides'], [{'id': 'override'}])
        self.assertEqual(answer['affected_tests'], [{'id': 'test'}])
        self.assertEqual(answer['changed_ids'], ['changed'])
        self.assertEqual(answer['_kin']['verdict']['limiting_factor'], 'reference_completion')
        self.assertEqual(len(answer.page_wires), 2)
        self.assertEqual(tuple(len(wire.encode('utf-8')) for wire in answer.page_wires),
                         answer.page_bytes)

    def test_impact_pages_reassemble_unicode_record_fragments(self):
        pages, expected = sample()
        for page in pages:
            page.pop('chain')
            page['affected_callers'] = []
            page['_kin']['page'] = {'version': 1, 'kind': 'impact', 'complete': False,
                                    'total_rows': 1, 'returned_rows': 0}
            page['_kin']['completeness']['limits'] = ['impact_page_partial']
            page['record_fragment']['collection'] = 'affected_tests'
        by_cursor = {None: pages[0], pages[0]['next_cursor']: pages[1],
                     pages[1]['next_cursor']: pages[2]}
        answer = drain_impact(lambda cursor: wire(by_cursor[cursor]), 2000)
        self.assertEqual(answer['affected_tests'], [expected['focal_entity']])

    def test_impact_pages_reject_lost_rows_changed_totals_and_wrong_kind(self):
        pages = self.impact_pages()
        pages[-1]['affected_tests'] = []
        pages[-1]['_kin']['page']['returned_rows'] = 1
        with self.assertRaisesRegex(ValueError, 'lost bucket rows'):
            self.drain_impact_pages(pages)
        pages = self.impact_pages()
        pages[-1]['_kin']['page']['total_rows'] = 2
        with self.assertRaisesRegex(ValueError, 'row total changed'):
            self.drain_impact_pages(pages)
        pages = self.impact_pages()
        pages[0]['_kin']['page']['kind'] = 'references'
        with self.assertRaisesRegex(ValueError, 'another page kind'):
            self.drain_impact_pages(pages)

    def test_impact_pages_require_impact_partial_limit(self):
        pages = self.impact_pages()
        pages[0]['_kin']['completeness']['limits'] = ['reference_page_partial']
        with self.assertRaisesRegex(ValueError, 'does not say it is partial'):
            self.drain_impact_pages(pages)

    def test_reference_pages_preserve_nested_candidates_and_safety_readings(self):
        pages, expected = sample()
        for page in pages:
            page.pop('chain')
            page['references'] = []
            page['_kin']['completeness']['limits'] = ['reference_page_partial']
            page['_kin']['page'] = {'version': 1, 'kind': 'references',
                                    'complete': False, 'total_references': 2}
        pages[0]['references'] = [{'entity_id': 'caller-a'}]
        pages[1]['references'] = [{'entity_id': 'caller-b'}]
        pages[1]['call_sites'] = {'candidates': [{'caller': 'uncertain'}]}
        pages[-1]['readings'] = [
            {'key': 'call_sites', 'value': {'candidate_count': 1, 'clauses': ['unresolved']}},
            {'key': '_kin', 'value': {'verdict': {'state': 'inconclusive'}}},
        ]
        by_cursor = {None: pages[0], pages[0]['next_cursor']: pages[1],
                     pages[1]['next_cursor']: pages[2]}
        answer = drain_references(lambda cursor: wire(by_cursor[cursor]), 2000)
        self.assertEqual(answer['focal_entity'], expected['focal_entity'])
        self.assertEqual(answer['references'], [{'entity_id': 'caller-a'}, {'entity_id': 'caller-b'}])
        self.assertEqual(answer['call_sites'], {'candidate_count': 1, 'clauses': ['unresolved'],
                                               'candidates': [{'caller': 'uncertain'}]})
        self.assertEqual(answer['_kin']['verdict']['state'], 'inconclusive')
        self.assertEqual(len(answer.page_bytes), 3)
        pages[1]['references'] = []
        with self.assertRaisesRegex(ValueError, 'lost caller rows'):
            drain_references(lambda cursor: wire(by_cursor[cursor]), 2000)

    def test_reference_drain_rejects_another_tool_page(self):
        pages, _ = sample()
        with self.assertRaisesRegex(ValueError, 'another page kind'):
            drain_references(lambda cursor: wire(pages[0]), 2000)

    def test_unicode_fields_and_absolute_parentage_are_preserved(self):
        pages, expected = sample()
        result = drain(pages)
        self.assertEqual(result, expected)
        self.assertEqual(len(result.page_bytes), 3)
        self.assertTrue(all(size <= 2000 for size in result.page_bytes))

    def test_completed_field_advances_to_the_next_cursor(self):
        pages, _ = sample()
        pages = pages[1:]
        fragment = pages[0]['record_fragment']
        fragment.update(byte_offset=0, text='proof', total_bytes=5)
        requested = []
        by_cursor = {None: pages[0], pages[0]['next_cursor']: pages[1]}

        def fetch(cursor):
            requested.append(cursor)
            return wire(by_cursor[cursor])

        result = drain_trace(fetch, 2000)
        self.assertEqual(requested, [None, pages[0]['next_cursor']])
        self.assertEqual(result['focal_entity'], {'body': 'proof', 'id': 'one'})
        self.assertEqual(len(result.page_bytes), 2)

    def test_completed_field_still_validates_its_cursor(self):
        pages, _ = sample()
        pages[1]['next_cursor'] = 'x' * 257
        with self.assertRaisesRegex(ValueError, 'cursor is malformed'):
            drain(pages)

    def test_repeated_complete_field_is_rejected(self):
        pages, _ = sample()
        first = copy.deepcopy(pages[1])
        first['record_fragment'].update(byte_offset=0, text='proof', total_bytes=5)
        first['next_cursor'] = 'next-field'
        repeated = copy.deepcopy(first)
        repeated['next_cursor'] = 'last-field'
        with self.assertRaisesRegex(ValueError, 'field appeared twice'):
            drain([first, repeated, pages[-1]])

    def test_final_page_cannot_certify_absence(self):
        pages, _ = sample()
        pages[-1]['negative']['safe_to_conclude_absent'] = True
        with self.assertRaisesRegex(ValueError, 'certifies absence'):
            drain(pages)

    def test_partial_page_keeps_the_answers_state_but_must_say_it_is_partial(self):
        pages, expected = sample()
        for page in pages:
            page['_kin']['verdict']['state'] = 'certified'
        self.assertEqual(drain(pages), expected)
        pages, _ = sample()
        pages[1]['_kin']['verdict']['safe_to_conclude_absent'] = True
        with self.assertRaisesRegex(ValueError, 'unqualified verdict'):
            drain(pages)
        pages, _ = sample()
        pages[1]['_kin']['completeness']['limits'] = []
        with self.assertRaisesRegex(ValueError, 'does not say it is partial'):
            drain(pages)

    def test_wire_overrun_is_not_hidden_by_reassembly(self):
        pages, _ = sample()
        pages[0]['unexpected_padding'] = 'a' * 2000
        with self.assertRaisesRegex(ValueError, 'wire bytes'):
            drain(pages)

    def test_fragment_overlap_is_rejected(self):
        pages, _ = sample()
        pages[1]['record_fragment']['byte_offset'] -= 1
        with self.assertRaisesRegex(ValueError, 'gap or overlap'):
            drain(pages)

    def test_unfinished_record_is_rejected(self):
        pages, _ = sample()
        pages[-1].pop('record_fragment')
        with self.assertRaisesRegex(ValueError, 'unfinished'):
            drain(pages)

    def test_missing_chain_step_is_rejected(self):
        pages, _ = sample()
        pages[-1]['chain'] = []
        with self.assertRaisesRegex(ValueError, 'lost chain steps'):
            drain(pages)


if __name__ == '__main__':
    unittest.main()
